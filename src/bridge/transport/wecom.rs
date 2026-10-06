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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
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
use super::session_store::{session_store_for, SessionStore};
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

// ── CLI 会话 store ────────────────────────────────────────────────────────────

/// `req_id → chatid` 映射的上限：`req_id` 只在「该轮回复期间」有用，
/// 长期运行的进程不能让它无限增长；FIFO 淘汰天然保留最近若干轮。
const WECOM_REQ_MAP_MAX: usize = 1024;

#[derive(Default)]
struct WecomConv {
    by_req: HashMap<String, String>,
    order: VecDeque<String>,
}

/// wecom 的 `context_token` 是**每条消息一个**的 `req_id`（回复必须逐字回填，
/// MCP 出站也用它做投递地址，不能改），而 CLI 会话属于「会话」——
/// 所以 store 键用 `chatid`，这张表把回复 token 映射回会话键。
struct WecomSessions {
    store: Arc<SessionStore>,
    conv: std::sync::Mutex<WecomConv>,
}

impl WecomSessions {
    /// `Some(path)` → 落盘的 CLI 会话 store（`None` = 纯内存）。
    fn with_store_path(store_path: Option<PathBuf>) -> Self {
        Self {
            store: session_store_for(store_path),
            conv: std::sync::Mutex::new(WecomConv::default()),
        }
    }

    /// 登记 `req_id → chatid`，再按 chatid 查上一轮的 CLI 会话号。
    fn inbound_session(&self, req_id: &str, chat_id: &str) -> Option<String> {
        {
            let mut conv = self.conv.lock().unwrap_or_else(|e| e.into_inner());
            if !conv.by_req.contains_key(req_id) {
                conv.order.push_back(req_id.to_string());
                while conv.order.len() > WECOM_REQ_MAP_MAX {
                    if let Some(oldest) = conv.order.pop_front() {
                        conv.by_req.remove(&oldest);
                    }
                }
            }
            conv.by_req.insert(req_id.to_string(), chat_id.to_string());
        }
        self.store.lookup("wecom", chat_id)
    }

    /// 反查 `req_id → chatid` 后写入 store；未知 `req_id`（回复晚于淘汰、
    /// 或不是本进程产生的投递地址）只记 debug 并跳过。
    fn persist_reply_session(&self, req_id: &str, cli_session_id: Option<&str>) {
        let chat_id = {
            let conv = self.conv.lock().unwrap_or_else(|e| e.into_inner());
            conv.by_req.get(req_id).cloned()
        };
        match chat_id {
            Some(chat_id) => self.store.remember("wecom", &chat_id, cli_session_id),
            None => debug!(
                req_id,
                "WeCom: reply for an unknown req_id; cli_session_id not persisted"
            ),
        }
    }
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
    sessions: Arc<WecomSessions>,
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

        let chat_id = body
            .get("chatid")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
        let session_id = chat_id
            .as_deref()
            .and_then(|chat_id| self.sessions.inbound_session(&req_id, chat_id));
        // `context_token` 是本条消息的 `req_id`（回复逐字回填），同一 chat 的并发
        // 消息因此会各建一个 worker、并发跑 CLI；`dispatch_key` 取稳定的 chatid，
        // 让同一会话的消息在 dispatcher 里按到达顺序串行。
        let dispatch_key = chat_id.as_deref().map(|chat_id| format!("wecom:{chat_id}"));

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
            dispatch_key,
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
    /// 与 WS worker 共享：`req_id → chatid` 映射 + CLI 会话 store。
    sessions: Arc<WecomSessions>,
}

impl WecomTransport {
    /// Resolve adapter-owned credentials (profile `im_credentials.bot_id` /
    /// `bot_secret` → env `WECOM_BOT_ID` / `WECOM_BOT_SECRET`), session store
    /// kept in memory.
    pub fn from_credentials(creds: &HashMap<String, String>) -> Result<Self> {
        Self::from_credentials_with_store_path(creds, None)
    }

    /// Same as [`Self::from_credentials`], but persists the CLI session store to
    /// `store_path` when `Some`.
    pub fn from_credentials_with_store_path(
        creds: &HashMap<String, String>,
        store_path: Option<PathBuf>,
    ) -> Result<Self> {
        let bot_id = creds
            .get("bot_id")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| {
                std::env::var("WECOM_BOT_ID")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .context("transport: wecom 需要 im_credentials.bot_id 或环境变量 WECOM_BOT_ID")?;
        let bot_secret = creds
            .get("bot_secret")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or_else(|| {
                std::env::var("WECOM_BOT_SECRET")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .context(
                "transport: wecom 需要 im_credentials.bot_secret 或环境变量 WECOM_BOT_SECRET",
            )?;
        Ok(Self::new_with_session_store_path(
            bot_id, bot_secret, store_path,
        ))
    }

    /// Create the transport and spawn the background WebSocket worker.
    pub fn new(bot_id: String, bot_secret: String) -> Self {
        Self::new_with_session_store_path(bot_id, bot_secret, None)
    }

    /// [`Self::new`] plus an explicit session-store file path (`None` =
    /// in-memory only).
    pub fn new_with_session_store_path(
        bot_id: String,
        bot_secret: String,
        store_path: Option<PathBuf>,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client for WeCom");

        let sessions = Arc::new(WecomSessions::with_store_path(store_path));
        let worker = WecomWsWorker {
            bot_id,
            bot_secret,
            http: http.clone(),
            inbound_tx,
            reply_rx: Arc::new(Mutex::new(reply_rx)),
            access_token_cache: Arc::new(Mutex::new(None)),
            sessions: sessions.clone(),
        };

        tokio::spawn(worker.run());

        Self {
            inbound_rx: Mutex::new(inbound_rx),
            reply_tx,
            http,
            sessions,
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
            let req_id = reply.context_token;
            // Persist before the empty-text early return: a streaming turn's
            // "persist-only" reply has an empty body + a `cli_session_id`.
            self.sessions
                .persist_reply_session(&req_id, reply.cli_session_id.as_deref());
            if reply.text.trim().is_empty() {
                return Ok(SendOutcome::Sent);
            }
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
        TransportCapabilities {
            media_upload: true,
            max_text_len: None,
        }
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

#[cfg(test)]
mod session_store_tests {
    use super::*;

    fn callback(req_id: &str, chat_id: &str) -> IncomingMsg {
        serde_json::from_value(serde_json::json!({
            "cmd": "aibot_msg_callback",
            "headers": { "req_id": req_id },
            "body": {
                "msgtype": "text",
                "text": { "content": "hi" },
                "chatid": chat_id,
                "chattype": "single",
                "from": { "userid": "u_1" }
            }
        }))
        .expect("incoming msg")
    }

    fn worker_for(sessions: Arc<WecomSessions>) -> WecomWsWorker {
        let (inbound_tx, _inbound_rx) = mpsc::unbounded_channel();
        let (_reply_tx, reply_rx) = mpsc::unbounded_channel();
        WecomWsWorker {
            bot_id: "bot".into(),
            bot_secret: "secret".into(),
            http: reqwest::Client::new(),
            inbound_tx,
            reply_rx: Arc::new(Mutex::new(reply_rx)),
            access_token_cache: Arc::new(Mutex::new(None)),
            sessions,
        }
    }

    async fn inbound_msg(
        sessions: &Arc<WecomSessions>,
        req_id: &str,
        chat_id: &str,
    ) -> InboundMessage {
        let worker = worker_for(sessions.clone());
        worker
            .wecom_callback_to_inbound(&callback(req_id, chat_id))
            .await
            .expect("inbound message")
    }

    async fn inbound_session(
        sessions: &Arc<WecomSessions>,
        req_id: &str,
        chat_id: &str,
    ) -> Option<String> {
        inbound_msg(sessions, req_id, chat_id).await.session_id
    }

    #[tokio::test]
    async fn same_chat_id_shares_one_dispatch_key() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));
        let first = inbound_msg(&sessions, "req-1", "chat-1").await;
        let second = inbound_msg(&sessions, "req-2", "chat-1").await;

        // context_token 逐条不同（回复逐字回填），dispatch_key 必须相同：
        // 否则同一会话的两条消息会各起一个 worker 并发跑 CLI。
        assert_ne!(first.context_token, second.context_token);
        assert_eq!(first.dispatch_key.as_deref(), Some("wecom:chat-1"));
        assert_eq!(first.dispatch_key, second.dispatch_key);
    }

    #[tokio::test]
    async fn different_chat_ids_use_different_dispatch_keys() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));
        let a = inbound_msg(&sessions, "req-1", "chat-1").await;
        let b = inbound_msg(&sessions, "req-2", "chat-2").await;
        assert_ne!(a.dispatch_key, b.dispatch_key);
        assert_eq!(b.dispatch_key.as_deref(), Some("wecom:chat-2"));
    }

    #[tokio::test]
    async fn wecom_first_turn_has_no_resume_id() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));

        // 第 1 轮：store 空 → 首轮必须无 resume id（且不能是 chatid / req_id）。
        let turn = inbound_session(&sessions, "req-1", "chat-1").await;
        assert_eq!(turn, None);
        assert_ne!(turn.as_deref(), Some("chat-1"));
        assert_ne!(turn.as_deref(), Some("req-1"));
    }

    #[tokio::test]
    async fn wecom_session_survives_the_next_turn_with_a_new_req_id() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));

        assert_eq!(inbound_session(&sessions, "req-1", "chat-1").await, None);

        sessions.persist_reply_session("req-1", Some("cli-5"));

        // 第 2 轮：不同 req_id、同一 chatid → 命中。若 store 键用 req_id 必 miss。
        assert_eq!(
            inbound_session(&sessions, "req-2", "chat-1")
                .await
                .as_deref(),
            Some("cli-5")
        );
    }

    #[tokio::test]
    async fn wecom_blank_cli_session_id_does_not_clobber() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));
        inbound_session(&sessions, "req-1", "chat-1").await;
        sessions.persist_reply_session("req-1", Some("cli-5"));

        sessions.persist_reply_session("req-1", Some("  "));
        sessions.persist_reply_session("req-1", None);
        assert_eq!(
            inbound_session(&sessions, "req-3", "chat-1")
                .await
                .as_deref(),
            Some("cli-5")
        );
    }

    #[tokio::test]
    async fn wecom_unknown_req_id_does_not_persist_or_panic() {
        let sessions = Arc::new(WecomSessions::with_store_path(None));
        sessions.persist_reply_session("unknown-req", Some("cli-9"));
        assert_eq!(inbound_session(&sessions, "req-1", "chat-1").await, None);
    }

    #[test]
    fn wecom_session_survives_store_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wecom.sessions.json");

        let first = WecomSessions::with_store_path(Some(path.clone()));
        assert_eq!(first.inbound_session("req-1", "chat-1"), None);
        first.persist_reply_session("req-1", Some("cli-7"));
        drop(first);

        // 模拟 bridge 重启：同一 store 文件、全新 `req_id → chatid` 映射。
        let reopened = WecomSessions::with_store_path(Some(path));
        assert_eq!(
            reopened.inbound_session("req-2", "chat-1").as_deref(),
            Some("cli-7")
        );
    }

    #[test]
    fn wecom_req_id_map_is_bounded_and_evicts_oldest() {
        let sessions = WecomSessions::with_store_path(None);
        for i in 0..=WECOM_REQ_MAP_MAX {
            sessions.inbound_session(&format!("req-{i}"), "chat-1");
        }
        let conv = sessions.conv.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(conv.order.len(), WECOM_REQ_MAP_MAX);
        assert_eq!(conv.by_req.len(), WECOM_REQ_MAP_MAX);
        assert!(
            !conv.by_req.contains_key("req-0"),
            "最旧的 req_id 必须被淘汰"
        );
        assert!(conv
            .by_req
            .contains_key(&format!("req-{WECOM_REQ_MAP_MAX}")));
    }
}
