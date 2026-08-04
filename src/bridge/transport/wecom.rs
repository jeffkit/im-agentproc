//! 企业微信智能机器人 WebSocket 长连接 Transport 适配器。
//!
//! 通过 `wss://openws.work.weixin.qq.com` 建立 WebSocket 长连接，
//! 订阅智能机器人消息（aibot_subscribe），接收 `aibot_msg_callback`，
//! 并通过同一连接的 `aibot_respond_msg` 回复（无需公网 URL）。
//!
//! 配置 (`im_credentials:`):
//! - `bot_id`:     企业微信智能机器人的 BotID
//! - `bot_secret`: 长连接专用密钥 Secret
//!
//! 参考文档：
//! <https://developer.work.weixin.qq.com/document/path/101463>

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::media::download_to_temp;
use super::{
    InboundMessage, InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};

const WS_URL: &str = "wss://openws.work.weixin.qq.com";
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const RECONNECT_BASE_SECS: u64 = 5;
const RECONNECT_MAX_SECS: u64 = 60;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct WsCmd<B: Serialize> {
    cmd: &'static str,
    headers: WsHeaders,
    body: B,
}

#[derive(Debug, Serialize, Deserialize)]
struct WsHeaders {
    req_id: String,
}

#[derive(Debug, Serialize)]
struct SubscribeBody {
    bot_id: String,
    secret: String,
}

#[derive(Debug, Serialize)]
struct RespondTextBody {
    msgtype: &'static str,
    text: RespondText,
}

#[derive(Debug, Serialize)]
struct RespondText {
    content: String,
}

#[derive(Debug, Serialize)]
struct RespondMediaBody {
    msgtype: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<RespondMediaPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<RespondMediaPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<RespondMediaPayload>,
}

#[derive(Debug, Serialize)]
struct RespondMediaPayload {
    /// Base64-encoded media bytes. WeCom aibot WS protocol accepts inline
    /// base64 for image / voice / file.
    media_base64: String,
}

#[derive(Debug, Deserialize)]
struct IncomingMsg {
    cmd: String,
    headers: Option<WsHeaders>,
    body: Option<serde_json::Value>,
    #[serde(default)]
    errcode: Option<i64>,
    #[serde(default)]
    errmsg: Option<String>,
}

const WECOM_API: &str = "https://qyapi.weixin.qq.com/cgi-bin";

// ── Access token cache ────────────────────────────────────────────────────────

struct WecomAccessToken {
    token: String,
    created_at: Instant,
    expires_in: u64,
}

impl WecomAccessToken {
    fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() + 300 < self.expires_in
    }
}

#[derive(Deserialize)]
struct WecomTokenResp {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: Option<String>,
}

// ── Background WS task ───────────────────────────────────────────────────────

struct WecomWsWorker {
    bot_id: String,
    bot_secret: String,
    http: reqwest::Client,
    inbound_tx: mpsc::UnboundedSender<InboundOutcome>,
    reply_rx: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
    /// Cached REST API access_token for media downloads.
    access_token_cache: Arc<Mutex<Option<WecomAccessToken>>>,
}

impl WecomWsWorker {
    /// Run the worker loop: connect → subscribe → poll forever, reconnecting on error.
    async fn run(self) {
        let mut backoff = RECONNECT_BASE_SECS;
        loop {
            if let Err(e) = self.run_once().await {
                error!(error = %e, backoff_secs = backoff, "WeCom WS disconnected; reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(RECONNECT_MAX_SECS);
        }
    }

    async fn run_once(&self) -> Result<()> {
        info!(url = WS_URL, "WeCom WS: connecting");
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .context("WeCom WS connect")?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe (authenticate)
        let req_id = Uuid::new_v4().to_string();
        let subscribe = WsCmd {
            cmd: "aibot_subscribe",
            headers: WsHeaders {
                req_id: req_id.clone(),
            },
            body: SubscribeBody {
                bot_id: self.bot_id.clone(),
                secret: self.bot_secret.clone(),
            },
        };
        let sub_json = serde_json::to_string(&subscribe)?;
        write
            .send(WsMessage::Text(sub_json.into()))
            .await
            .context("WeCom WS subscribe send")?;

        // Read subscribe response
        let sub_resp_raw = tokio::time::timeout(Duration::from_secs(10), read.next())
            .await
            .context("WeCom WS subscribe timeout")?
            .context("WeCom WS subscribe: stream closed")?
            .context("WeCom WS subscribe recv")?;

        if let WsMessage::Text(t) = sub_resp_raw {
            let resp: IncomingMsg = serde_json::from_str(&t).unwrap_or_else(|_| IncomingMsg {
                cmd: String::new(),
                headers: None,
                body: None,
                errcode: Some(-1),
                errmsg: Some(t.to_string()),
            });
            let code = resp.errcode.unwrap_or(0);
            if code != 0 {
                anyhow::bail!(
                    "WeCom aibot_subscribe failed (errcode={code}): {}",
                    resp.errmsg.unwrap_or_default()
                );
            }
            info!("WeCom WS: subscribed successfully");
        } else {
            anyhow::bail!("WeCom WS: unexpected subscribe response frame type");
        }

        // Reset reconnect backoff on successful connection
        let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await; // consume the first immediate tick

        let reply_rx = self.reply_rx.clone();

        loop {
            let mut reply_guard = reply_rx.lock().await;
            tokio::select! {
                biased;
                // Drain queued reply messages first
                reply_json = reply_guard.recv() => {
                    drop(reply_guard);
                    match reply_json {
                        Some(json) => {
                            write.send(WsMessage::Text(json.into())).await.context("WeCom WS send reply")?;
                        }
                        None => {
                            // Sender dropped — transport is shutting down
                            return Ok(());
                        }
                    }
                }
                // Inbound WS message
                frame = read.next() => {
                    drop(reply_guard);
                    match frame {
                        Some(Ok(WsMessage::Text(t))) => {
                            self.handle_text(t.as_str()).await;
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            write.send(WsMessage::Pong(data)).await.ok();
                        }
                        Some(Ok(WsMessage::Close(frame))) => {
                            info!(frame = ?frame, "WeCom WS: server closed connection");
                            anyhow::bail!("WeCom WS closed by server");
                        }
                        Some(Ok(_)) => {} // binary / pong / etc.
                        Some(Err(e)) => return Err(e.into()),
                        None => anyhow::bail!("WeCom WS stream ended"),
                    }
                }
                // Heartbeat ping
                _ = heartbeat.tick() => {
                    drop(reply_guard);
                    write.send(WsMessage::Ping(vec![].into())).await.context("WeCom WS ping")?;
                    debug!("WeCom WS: heartbeat ping sent");
                }
            }
        }
    }

    async fn get_access_token(&self) -> Result<String> {
        let mut cache = self.access_token_cache.lock().await;
        if let Some(ref t) = *cache {
            if t.is_valid() {
                return Ok(t.token.clone());
            }
        }
        debug!("WeCom: refreshing aibot access_token");
        let url = format!("{WECOM_API}/aibot/gettoken");
        let resp: WecomTokenResp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"botid": self.bot_id, "botsecret": self.bot_secret}))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("WeCom gettoken HTTP")?
            .json()
            .await
            .context("WeCom gettoken JSON")?;
        if resp.errcode != 0 {
            anyhow::bail!(
                "WeCom aibot gettoken failed (code={}): {}",
                resp.errcode,
                resp.errmsg.unwrap_or_default()
            );
        }
        let token = resp
            .access_token
            .context("WeCom gettoken: no access_token")?;
        let expires_in = resp.expires_in.unwrap_or(7200);
        *cache = Some(WecomAccessToken {
            token: token.clone(),
            created_at: Instant::now(),
            expires_in,
        });
        Ok(token)
    }

    async fn download_wecom_media(
        &self,
        media_id: &str,
        kind: &str,
        filename: Option<&str>,
        mime: Option<&str>,
    ) -> Result<MediaRef> {
        let token = self.get_access_token().await?;
        let url = format!(
            "{WECOM_API}/media/get?access_token={}&media_id={}",
            token, media_id
        );
        download_to_temp(&self.http, &url, None, kind, filename, mime).await
    }

    async fn handle_text(&self, raw: &str) {
        let msg: IncomingMsg = match serde_json::from_str(raw) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, raw = raw, "WeCom WS: failed to parse frame");
                return;
            }
        };

        match msg.cmd.as_str() {
            "aibot_msg_callback" => {
                if let Some(inbound) = self.wecom_callback_to_inbound(&msg).await {
                    let _ = self
                        .inbound_tx
                        .send(InboundOutcome::Messages(vec![inbound]));
                }
            }
            "aibot_event_callback" => {
                let is_disconnected = msg
                    .body
                    .as_ref()
                    .and_then(|b| b.get("event"))
                    .and_then(|e| e.get("eventtype"))
                    .and_then(|t| t.as_str())
                    == Some("disconnected_event");
                if is_disconnected {
                    warn!("WeCom WS: received disconnected_event (new connection kicked this one)");
                    let _ = self.inbound_tx.send(InboundOutcome::TokenRejected);
                }
            }
            _ => {
                debug!(cmd = %msg.cmd, "WeCom WS: ignoring unknown cmd");
            }
        }
    }

    async fn wecom_callback_to_inbound(&self, msg: &IncomingMsg) -> Option<InboundMessage> {
        let body = msg.body.as_ref()?;
        let req_id = msg.headers.as_ref().map(|h| h.req_id.clone())?;

        let msgtype = body.get("msgtype")?.as_str()?;

        let mut text: Option<String> = None;
        let mut media: Vec<MediaRef> = vec![];

        match msgtype {
            "text" => {
                text = body
                    .get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());
            }
            "image" => {
                if let Some(media_id) = body
                    .get("image")
                    .and_then(|i| i.get("media_id"))
                    .and_then(|m| m.as_str())
                {
                    match self
                        .download_wecom_media(media_id, "image", None, Some("image/jpeg"))
                        .await
                    {
                        Ok(r) => media.push(r),
                        Err(e) => warn!(error = %e, "WeCom image download failed"),
                    }
                }
            }
            "voice" => {
                if let Some(media_id) = body
                    .get("voice")
                    .and_then(|v| v.get("media_id"))
                    .and_then(|m| m.as_str())
                {
                    match self
                        .download_wecom_media(media_id, "audio", None, Some("audio/amr"))
                        .await
                    {
                        Ok(r) => media.push(r),
                        Err(e) => warn!(error = %e, "WeCom voice download failed"),
                    }
                }
            }
            "file" => {
                let file_val = body.get("file");
                let media_id = file_val
                    .and_then(|f| f.get("media_id"))
                    .and_then(|m| m.as_str());
                let filename = file_val
                    .and_then(|f| f.get("filename"))
                    .and_then(|n| n.as_str());
                if let Some(mid) = media_id {
                    match self.download_wecom_media(mid, "file", filename, None).await {
                        Ok(r) => media.push(r),
                        Err(e) => warn!(error = %e, "WeCom file download failed"),
                    }
                }
            }
            other => {
                debug!(msgtype = other, "WeCom: ignoring unsupported message type");
                return None;
            }
        }

        // Require text OR media; media-only messages get a placeholder.
        let text = match text {
            Some(t) => Some(t),
            None if !media.is_empty() => Some(
                media
                    .iter()
                    .map(|m| format!("[{}]", m.kind))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            None => return None,
        };

        let from_user = body
            .get("from")
            .and_then(|f| f.get("userid"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());

        let session_id = body
            .get("chatid")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());

        let session_name = body
            .get("chattype")
            .and_then(|t| t.as_str())
            .map(|t| format!("wecom-{t}"));

        Some(InboundMessage {
            context_token: Some(req_id),
            from_user,
            is_from_bot: false,
            text,
            media,
            session_id,
            session_name,
            a2a_call_id: None,
            extra: body.clone(),
            raw: serde_json::to_value(msg.body.as_ref()).unwrap_or(serde_json::Value::Null),
        })
    }
}

// ── Transport ────────────────────────────────────────────────────────────────

/// WeCom smart-bot WebSocket transport.
pub struct WecomTransport {
    inbound_rx: Mutex<mpsc::UnboundedReceiver<InboundOutcome>>,
    reply_tx: mpsc::UnboundedSender<String>,
    /// Owned by the transport (separate from the worker's copy) so the
    /// outbound media path can fetch remote `http(s)://` URLs without going
    /// through the WebSocket worker.
    http: reqwest::Client,
}

impl WecomTransport {
    /// Create the transport and spawn the background WebSocket worker.
    pub fn new(bot_id: String, bot_secret: String) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client for WeCom");

        let worker = WecomWsWorker {
            bot_id,
            bot_secret,
            http: http.clone(),
            inbound_tx,
            reply_rx: Arc::new(Mutex::new(reply_rx)),
            access_token_cache: Arc::new(Mutex::new(None)),
        };

        tokio::spawn(worker.run());

        Self {
            inbound_rx: Mutex::new(inbound_rx),
            reply_tx,
            http,
        }
    }
}

impl Transport for WecomTransport {
    fn next_inbound<'a>(&'a self, _buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            let mut rx = self.inbound_rx.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("WeCom inbound channel closed (worker exited)"))
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        Box::pin(async move {
            if reply.text.trim().is_empty() {
                return Ok(SendOutcome::Sent);
            }
            let req_id = reply.context_token;
            let cmd = WsCmd {
                cmd: "aibot_respond_msg",
                headers: WsHeaders { req_id },
                body: RespondTextBody {
                    msgtype: "text",
                    text: RespondText {
                        content: reply.text,
                    },
                },
            };
            let json = serde_json::to_string(&cmd).context("WeCom serialize reply")?;
            self.reply_tx
                .send(json)
                .map_err(|_| anyhow::anyhow!("WeCom reply channel closed"))?;
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "wecom"
    }

    fn send_media<'a>(
        &'a self,
        ctx: MediaOut,
        media: MediaRef,
    ) -> BoxFuture<'a, Result<SendOutcome>> {
        let http = self.http.clone();
        let reply_tx = self.reply_tx.clone();
        Box::pin(async move {
            // Read the bytes from any supported URI scheme.
            let bytes = super::media::read_media_bytes(&http, &media).await?;
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            // WeCom aibot WS reply supports `msgtype` ∈ {image, voice, file}
            // with a base64 media payload. Map our kind into the closest
            // match; everything that isn't image/voice collapses to `file`.
            let msgtype = match media.kind.as_str() {
                "image" => "image",
                "audio" | "voice" => "voice",
                _ => "file",
            };
            let payload = RespondMediaPayload { media_base64: b64 };
            let body = match msgtype {
                "image" => RespondMediaBody {
                    msgtype,
                    image: Some(payload),
                    voice: None,
                    file: None,
                },
                "voice" => RespondMediaBody {
                    msgtype,
                    image: None,
                    voice: Some(payload),
                    file: None,
                },
                _ => RespondMediaBody {
                    msgtype,
                    image: None,
                    voice: None,
                    file: Some(payload),
                },
            };
            let cmd = WsCmd {
                cmd: "aibot_respond_msg",
                headers: WsHeaders {
                    req_id: ctx.context_token,
                },
                body,
            };
            let json = serde_json::to_string(&cmd).context("WeCom serialize media")?;
            reply_tx
                .send(json)
                .map_err(|_| anyhow::anyhow!("WeCom reply channel closed"))?;
            Ok(SendOutcome::Sent)
        })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities { media_upload: true }
    }
}

#[cfg(test)]
mod send_media_tests {
    use super::*;

    #[test]
    fn respond_media_body_image_serializes_only_image_field() {
        let body = RespondMediaBody {
            msgtype: "image",
            image: Some(RespondMediaPayload {
                media_base64: "AAAA".into(),
            }),
            voice: None,
            file: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        // `voice` and `file` are skipped when None.
        assert!(json.contains("\"msgtype\":\"image\""));
        assert!(json.contains("\"image\":{\"media_base64\":\"AAAA\"}"));
        assert!(!json.contains("voice"));
        assert!(!json.contains("\"file\""));
    }

    #[test]
    fn respond_media_body_voice_serializes_only_voice_field() {
        let body = RespondMediaBody {
            msgtype: "voice",
            image: None,
            voice: Some(RespondMediaPayload {
                media_base64: "VVVV".into(),
            }),
            file: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"msgtype\":\"voice\""));
        assert!(json.contains("\"voice\":{\"media_base64\":\"VVVV\"}"));
        assert!(!json.contains("\"image\""));
        assert!(!json.contains("\"file\""));
    }

    #[test]
    fn respond_media_body_file_serializes_only_file_field() {
        let body = RespondMediaBody {
            msgtype: "file",
            image: None,
            voice: None,
            file: Some(RespondMediaPayload {
                media_base64: "RkZGRg==".into(),
            }),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"msgtype\":\"file\""));
        assert!(json.contains("\"file\":{\"media_base64\":\"RkZGRg==\"}"));
        assert!(!json.contains("\"image\""));
        assert!(!json.contains("\"voice\""));
    }

    #[test]
    fn ws_cmd_round_trips_for_image() {
        let cmd = WsCmd {
            cmd: "aibot_respond_msg",
            headers: WsHeaders {
                req_id: "req-42".into(),
            },
            body: RespondMediaBody {
                msgtype: "image",
                image: Some(RespondMediaPayload {
                    media_base64: "AAAA".into(),
                }),
                voice: None,
                file: None,
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        // Spot-check the four things the WeCom aibot gateway cares about:
        // cmd / headers.req_id / body.msgtype / body.image.media_base64.
        assert!(json.contains("\"cmd\":\"aibot_respond_msg\""));
        assert!(json.contains("\"req_id\":\"req-42\""));
        assert!(json.contains("\"msgtype\":\"image\""));
        assert!(json.contains("\"media_base64\":\"AAAA\""));
    }
}
