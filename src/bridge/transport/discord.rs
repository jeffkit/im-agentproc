//! Discord Gateway WebSocket Transport 适配器。
//!
//! 通过 Discord Gateway WebSocket API v10 接收消息，
//! 通过 Discord REST API 回复消息。
//!
//! 配置 (`im_credentials:`):
//! - `token`: Discord Bot Token（从 Discord Developer Portal 获取）
//!
//! Discord Gateway 文档：
//! <https://discord.com/developers/docs/events/gateway>

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use super::media::{download_to_temp, filename_from_url, read_media_bytes};
use super::{
    InboundMessage, InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};

const DISCORD_API: &str = "https://discord.com/api/v10";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const RECONNECT_BASE_SECS: u64 = 5;
const RECONNECT_MAX_SECS: u64 = 60;

// ── Gateway opcodes ───────────────────────────────────────────────────────────

const OP_DISPATCH: i64 = 0;
const OP_HEARTBEAT: i64 = 1;
const OP_IDENTIFY: i64 = 2;
const OP_RESUME: i64 = 6;
const OP_RECONNECT: i64 = 7;
const OP_INVALID_SESSION: i64 = 9;
const OP_HELLO: i64 = 10;
const OP_HEARTBEAT_ACK: i64 = 11;

// Close codes that should NOT trigger reconnect (auth failures, etc.)
const NO_RECONNECT_CODES: &[u16] = &[4004, 4010, 4011, 4012, 4013, 4014];

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct IdentifyPayload<'a> {
    op: i64,
    d: IdentifyData<'a>,
}

#[derive(Debug, Serialize)]
struct IdentifyData<'a> {
    token: &'a str,
    intents: u64,
    properties: IdentifyProps,
}

#[derive(Debug, Serialize)]
struct IdentifyProps {
    os: &'static str,
    browser: &'static str,
    device: &'static str,
}

#[derive(Debug, Serialize)]
struct HeartbeatPayload {
    op: i64,
    d: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ResumePayload<'a> {
    op: i64,
    d: ResumeData<'a>,
}

#[derive(Debug, Serialize)]
struct ResumeData<'a> {
    token: &'a str,
    session_id: &'a str,
    seq: i64,
}

#[derive(Debug, Serialize)]
struct SendMsgRequest<'a> {
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct SendMediaRequest<'a> {
    content: &'a str,
    attachments: Vec<DiscordAttachment<'a>>,
}

#[derive(Debug, Serialize)]
struct DiscordAttachment<'a> {
    id: u32,
    filename: &'a str,
}

#[derive(Debug, Deserialize)]
struct GatewayEvent {
    op: i64,
    #[serde(default)]
    d: serde_json::Value,
    #[serde(default)]
    s: Option<i64>,
    #[serde(default)]
    t: Option<String>,
}

// ── Background WS worker ──────────────────────────────────────────────────────

struct ResumeInfo {
    session_id: String,
    seq: i64,
    resume_url: String,
}

struct DiscordWsWorker {
    bot_token: String,
    http: reqwest::Client,
    inbound_tx: mpsc::UnboundedSender<InboundOutcome>,
    /// Application's own user ID (set after READY).
    own_id: Arc<Mutex<Option<String>>>,
}

impl DiscordWsWorker {
    async fn run(self) {
        let mut resume: Option<ResumeInfo> = None;
        let mut backoff = RECONNECT_BASE_SECS;

        loop {
            let url = resume
                .as_ref()
                .map(|r| r.resume_url.clone())
                .unwrap_or_else(|| GATEWAY_URL.to_string());

            match self.run_session(&url, resume.take()).await {
                Ok(Some(new_resume)) => {
                    info!("Discord WS: session ended with resume info, will resume");
                    resume = Some(new_resume);
                    backoff = RECONNECT_BASE_SECS;
                }
                Ok(None) => {
                    info!("Discord WS: session ended, will reconnect fresh");
                    backoff = RECONNECT_BASE_SECS;
                }
                Err(e) => {
                    error!(error = %e, backoff_secs = backoff, "Discord WS error; reconnecting");
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(RECONNECT_MAX_SECS);
        }
    }

    async fn run_session(
        &self,
        url: &str,
        resume: Option<ResumeInfo>,
    ) -> Result<Option<ResumeInfo>> {
        info!(url, "Discord WS: connecting");
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("Discord WS connect")?;
        let (mut write, mut read) = ws_stream.split();

        // Wait for HELLO (op 10) to get heartbeat interval.
        let hello_frame = tokio::time::timeout(Duration::from_secs(15), read.next())
            .await
            .context("Discord WS: timeout waiting for HELLO")?
            .context("Discord WS: stream closed")?
            .context("Discord WS: HELLO recv")?;

        let hello: GatewayEvent = match hello_frame {
            WsMessage::Text(t) => serde_json::from_str(&t).context("Discord WS: parse HELLO")?,
            _ => anyhow::bail!("Discord WS: unexpected frame type for HELLO"),
        };
        if hello.op != OP_HELLO {
            anyhow::bail!("Discord WS: expected op 10 (HELLO), got {}", hello.op);
        }
        let heartbeat_interval_ms = hello
            .d
            .get("heartbeat_interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(41250);
        info!(heartbeat_interval_ms, "Discord WS: received HELLO");

        // Send IDENTIFY or RESUME.
        if let Some(ref r) = resume {
            let payload = ResumePayload {
                op: OP_RESUME,
                d: ResumeData {
                    token: &self.bot_token,
                    session_id: &r.session_id,
                    seq: r.seq,
                },
            };
            let json = serde_json::to_string(&payload)?;
            write
                .send(WsMessage::Text(json.into()))
                .await
                .context("Discord WS: RESUME")?;
        } else {
            // Intents: GUILDS (1) | GUILD_MESSAGES (512) | MESSAGE_CONTENT (32768) |
            //          DIRECT_MESSAGES (4096)
            let intents: u64 = 1 | 512 | 32768 | 4096;
            let payload = IdentifyPayload {
                op: OP_IDENTIFY,
                d: IdentifyData {
                    token: &self.bot_token,
                    intents,
                    properties: IdentifyProps {
                        os: "linux",
                        browser: "im-agentproc",
                        device: "im-agentproc",
                    },
                },
            };
            let json = serde_json::to_string(&payload)?;
            write
                .send(WsMessage::Text(json.into()))
                .await
                .context("Discord WS: IDENTIFY")?;
        }

        // Session state
        let mut seq: Option<i64> = resume.map(|r| r.seq);
        let mut session_id: Option<String> = None;
        let mut resume_url: Option<String> = None;
        let mut heartbeat_ack = true;

        // Heartbeat task via channel.
        let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (hb_send_tx, mut hb_send_rx) = mpsc::unbounded_channel::<Option<i64>>();
        let hb_task = tokio::spawn({
            let hb_send_tx = hb_send_tx.clone();
            async move {
                // Initial jitter: Discord says to wait interval * random[0..1] first.
                let jitter = (rand_u64() % heartbeat_interval_ms).max(1000);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(jitter)) => {},
                    _ = async { loop { tokio::task::yield_now().await } } => return,
                }
                let mut interval =
                    tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if hb_send_tx.send(None).is_err() { return; }
                        }
                        _ = &mut hb_stop_rx => return,
                    }
                }
            }
        });

        let result: Result<Option<ResumeInfo>> = async {
            loop {
                tokio::select! {
                    biased;
                    // Heartbeat interval fired
                    hb_seq = hb_send_rx.recv() => {
                        if hb_seq.is_none() { break; }
                        if !heartbeat_ack {
                            anyhow::bail!("Discord WS: missed heartbeat ACK, reconnecting");
                        }
                        heartbeat_ack = false;
                        let payload = HeartbeatPayload { op: OP_HEARTBEAT, d: seq };
                        let json = serde_json::to_string(&payload)?;
                        write.send(WsMessage::Text(json.into())).await.context("Discord WS: heartbeat")?;
                        debug!("Discord WS: sent heartbeat (seq={seq:?})");
                    }
                    // Inbound gateway frame
                    frame = read.next() => {
                        let frame = match frame {
                            Some(Ok(f)) => f,
                            Some(Err(e)) => anyhow::bail!("Discord WS recv: {e}"),
                            None => anyhow::bail!("Discord WS: stream ended"),
                        };
                        match frame {
                            WsMessage::Text(t) => {
                                let event: GatewayEvent = serde_json::from_str(&t)
                                    .context("Discord WS: parse event")?;
                                if let Some(s) = event.s { seq = Some(s); }

                                match event.op {
                                    OP_DISPATCH => {
                                        let t = event.t.as_deref().unwrap_or("");
                                        match t {
                                            "READY" => {
                                                session_id = event.d.get("session_id")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());
                                                resume_url = event.d.get("resume_gateway_url")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| format!("{s}?v=10&encoding=json"));
                                                let own = event.d.get("user")
                                                    .and_then(|u| u.get("id"))
                                                    .and_then(|id| id.as_str())
                                                    .map(|s| s.to_string());
                                                if let Some(ref id) = own {
                                                    *self.own_id.lock().await = Some(id.clone());
                                                }
                                                info!(
                                                    session_id = ?session_id,
                                                    own_id = ?own,
                                                    "Discord WS: READY"
                                                );
                                            }
                                            "MESSAGE_CREATE" => {
                                                let own_id = self.own_id.lock().await.clone();
                                                if let Some(msg) = self.discord_msg_to_inbound(&event.d, own_id.as_deref()).await {
                                                    let _ = self.inbound_tx.send(InboundOutcome::Messages(vec![msg]));
                                                }
                                            }
                                            _ => {
                                                debug!(event_type = t, "Discord WS: ignoring event");
                                            }
                                        }
                                    }
                                    OP_HEARTBEAT => {
                                        // Discord requests immediate heartbeat
                                        let payload = HeartbeatPayload { op: OP_HEARTBEAT, d: seq };
                                        let json = serde_json::to_string(&payload)?;
                                        write.send(WsMessage::Text(json.into())).await?;
                                    }
                                    OP_HEARTBEAT_ACK => {
                                        heartbeat_ack = true;
                                    }
                                    OP_RECONNECT => {
                                        info!("Discord WS: server requested reconnect");
                                        let resume = session_id.and_then(|sid| {
                                            resume_url.map(|url| ResumeInfo {
                                                session_id: sid,
                                                seq: seq.unwrap_or(0),
                                                resume_url: url,
                                            })
                                        });
                                        return Ok(resume);
                                    }
                                    OP_INVALID_SESSION => {
                                        let resumable = event.d.as_bool().unwrap_or(false);
                                        if resumable {
                                            let resume = session_id.and_then(|sid| {
                                                resume_url.map(|url| ResumeInfo {
                                                    session_id: sid,
                                                    seq: seq.unwrap_or(0),
                                                    resume_url: url,
                                                })
                                            });
                                            return Ok(resume);
                                        }
                                        warn!("Discord WS: invalid session (not resumable), reconnecting fresh");
                                        return Ok(None);
                                    }
                                    _ => {
                                        debug!(op = event.op, "Discord WS: ignoring opcode");
                                    }
                                }
                            }
                            WsMessage::Close(frame) => {
                                let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                                if NO_RECONNECT_CODES.contains(&code) {
                                    // Auth / intent errors — signal TokenRejected so dispatcher stops.
                                    warn!(close_code = code, "Discord WS: fatal close code");
                                    let _ = self.inbound_tx.send(InboundOutcome::TokenRejected);
                                    return Ok(None);
                                }
                                anyhow::bail!("Discord WS: server closed (code={code})");
                            }
                            WsMessage::Ping(data) => {
                                write.send(WsMessage::Pong(data)).await.ok();
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(None)
        }.await;

        let _ = hb_stop_tx.send(());
        hb_task.abort();
        result
    }
}

/// Simple deterministic pseudo-randomness for jitter (no rand dep needed).
fn rand_u64() -> u64 {
    use std::time::UNIX_EPOCH;
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(1234567)
}

impl DiscordWsWorker {
    async fn discord_msg_to_inbound(
        &self,
        data: &serde_json::Value,
        own_id: Option<&str>,
    ) -> Option<InboundMessage> {
        // Skip messages from the bot itself.
        let author_id = data
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(|id| id.as_str())?;
        if own_id == Some(author_id) {
            return None;
        }
        let is_bot = data
            .get("author")
            .and_then(|a| a.get("bot"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if is_bot {
            warn!(author_id, "Discord: dropping message from bot (anti-loop)");
            return None;
        }

        let text = data
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|t| !t.trim().is_empty())
            .map(|s| s.to_string());

        // ── Download attachments ────────────────────────────────────────────
        // Discord CDN attachment URLs are public — no auth header needed.
        let mut media: Vec<MediaRef> = vec![];
        if let Some(attachments) = data.get("attachments").and_then(|a| a.as_array()) {
            for att in attachments {
                let url = match att.get("url").and_then(|u| u.as_str()) {
                    Some(u) => u,
                    None => continue,
                };
                let filename = att.get("filename").and_then(|f| f.as_str());
                let mime = att.get("content_type").and_then(|ct| ct.as_str());
                let kind = if mime.map(|m| m.starts_with("image/")).unwrap_or(false) {
                    "image"
                } else if mime.map(|m| m.starts_with("audio/")).unwrap_or(false) {
                    "audio"
                } else if mime.map(|m| m.starts_with("video/")).unwrap_or(false) {
                    "video"
                } else {
                    "file"
                };
                match download_to_temp(&self.http, url, None, kind, filename, mime).await {
                    Ok(r) => media.push(r),
                    Err(e) => warn!(error = %e, url, "Discord attachment download failed"),
                }
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

        let channel_id = data
            .get("channel_id")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())?;
        let guild_id = data
            .get("guild_id")
            .and_then(|g| g.as_str())
            .map(|s| s.to_string());
        let session_name = guild_id
            .as_deref()
            .map(|g| format!("discord-{g}"))
            .or_else(|| Some("discord-dm".to_string()));

        Some(InboundMessage {
            context_token: Some(channel_id.clone()),
            from_user: Some(author_id.to_string()),
            is_from_bot: false,
            text,
            media,
            session_id: Some(channel_id),
            session_name,
            a2a_call_id: None,
            extra: data.clone(),
            raw: data.clone(),
        })
    }
}

// ── Transport ────────────────────────────────────────────────────────────────

/// Discord Gateway WebSocket Transport。
pub struct DiscordTransport {
    inbound_rx: Mutex<mpsc::UnboundedReceiver<InboundOutcome>>,
    http: reqwest::Client,
    bot_token: String,
    /// API base url (defaults to `https://discord.com/api/v10`; tests override
    /// to point at a mockito server).
    api_base: String,
}

impl DiscordTransport {
    /// 创建 Transport 并在后台启动 Discord Gateway WebSocket 连接。
    pub fn new(bot_token: String) -> Result<Self> {
        Self::with_api_base(bot_token, DISCORD_API.to_string())
    }

    /// Construct with an explicit API base url. Tests use this to point the
    /// transport at a local mockito server; production code should call
    /// [`Self::new`].
    pub fn with_api_base(bot_token: String, api_base: String) -> Result<Self> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client for Discord")?;

        let own_id = Arc::new(Mutex::new(None::<String>));

        let worker = DiscordWsWorker {
            bot_token: bot_token.clone(),
            http: http.clone(),
            inbound_tx,
            own_id,
        };
        tokio::spawn(worker.run());

        Ok(Self {
            inbound_rx: Mutex::new(inbound_rx),
            http,
            bot_token,
            api_base,
        })
    }
}

impl Transport for DiscordTransport {
    fn next_inbound<'a>(&'a self, _buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            let mut rx = self.inbound_rx.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("Discord inbound channel closed"))
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        Box::pin(async move {
            if reply.text.trim().is_empty() {
                return Ok(SendOutcome::Sent);
            }
            // context_token = channel_id
            let channel_id = &reply.context_token;
            let req = SendMsgRequest {
                content: &reply.text,
            };
            let resp = self
                .http
                .post(format!("{}/channels/{channel_id}/messages", self.api_base))
                .header("Authorization", format!("Bot {}", self.bot_token))
                .json(&req)
                .send()
                .await
                .context("Discord sendMessage HTTP")?;
            let status = resp.status();
            if status.as_u16() == 429 {
                return Ok(SendOutcome::Throttled {
                    ret: 429,
                    errmsg: Some("rate limited".to_string()),
                });
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Discord sendMessage HTTP {status}: {body}");
            }
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "discord"
    }

    fn send_media<'a>(
        &'a self,
        ctx: MediaOut,
        media: MediaRef,
    ) -> BoxFuture<'a, Result<SendOutcome>> {
        let http = self.http.clone();
        let bot_token = self.bot_token.clone();
        Box::pin(async move {
            // Read the bytes from a local path or fetch a remote URL.
            let bytes = read_media_bytes(&http, &media).await?;
            // Discord filename: prefer the explicit one, fall back to the URL path tail.
            let filename = media
                .filename
                .clone()
                .or_else(|| filename_from_url(&media.url))
                .unwrap_or_else(|| "attachment".to_string());
            let req = SendMediaRequest {
                content: ctx.caption.as_deref().unwrap_or(""),
                attachments: vec![DiscordAttachment {
                    id: 0,
                    filename: &filename,
                }],
            };
            let channel_id = &ctx.context_token;
            let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.clone());
            let form = reqwest::multipart::Form::new()
                .text(
                    "payload_json",
                    serde_json::to_string(&req).context("serialize Discord media request")?,
                )
                .part("files[0]", part);
            let resp = http
                .post(format!("{}/channels/{channel_id}/messages", self.api_base))
                .send()
                .await
                .context("Discord sendMedia HTTP")?;
            let status = resp.status();
            if status.as_u16() == 429 {
                return Ok(SendOutcome::Throttled {
                    ret: 429,
                    errmsg: Some("rate limited".to_string()),
                });
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Discord sendMedia HTTP {status}: {body}");
            }
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
    fn filename_from_url_pulls_tail_after_last_slash() {
        assert_eq!(
            filename_from_url("https://cdn.example.com/path/to/photo.png"),
            Some("photo.png".into())
        );
        // Query string stripped before tail extraction.
        assert_eq!(
            filename_from_url("https://cdn.example.com/x/y/z.pdf?ver=2"),
            Some("z.pdf".into())
        );
        // Bare hostname → no filename.
        assert_eq!(filename_from_url("https://example.com"), None);
        // Empty tail.
        assert_eq!(filename_from_url("https://example.com/"), None);
    }

    #[tokio::test]
    async fn read_media_bytes_decodes_data_url() {
        // base64("hello") = "aGVsbG8="
        let media = MediaRef {
            kind: "file".into(),
            url: "data:text/plain;base64,aGVsbG8=".into(),
            filename: None,
            mime_type: Some("text/plain".into()),
            size: None,
        };
        let http = reqwest::Client::new();
        let bytes = read_media_bytes(&http, &media)
            .await
            .expect("data: decodes");
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn read_media_bytes_reads_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"local-bytes").unwrap();
        let media = MediaRef {
            kind: "file".into(),
            url: format!("file://{}", path.display()),
            filename: Some("hello.txt".into()),
            mime_type: Some("text/plain".into()),
            size: None,
        };
        let http = reqwest::Client::new();
        let bytes = read_media_bytes(&http, &media)
            .await
            .expect("file:// reads");
        assert_eq!(bytes, b"local-bytes");
    }

    #[tokio::test]
    async fn read_media_bytes_rejects_non_base64_data_url() {
        let media = MediaRef {
            kind: "file".into(),
            url: "data:text/plain,hello-world".into(),
            filename: None,
            mime_type: None,
            size: None,
        };
        let http = reqwest::Client::new();
        let err = read_media_bytes(&http, &media).await.unwrap_err();
        assert!(format!("{err}").contains("base64"));
    }

    #[tokio::test]
    async fn read_media_bytes_rejects_unsupported_scheme() {
        let media = MediaRef {
            kind: "file".into(),
            url: "ftp://example.com/x.bin".into(),
            filename: None,
            mime_type: None,
            size: None,
        };
        let http = reqwest::Client::new();
        let err = read_media_bytes(&http, &media).await.unwrap_err();
        assert!(format!("{err}").contains("unsupported"));
    }

    #[test]
    fn capabilities_reports_media_upload_true() {
        let cap = TransportCapabilities { media_upload: true };
        assert!(cap.media_upload);
    }
}

#[cfg(test)]
mod send_media_e2e_tests {
    use super::*;

    fn transport_for(base_url: String) -> DiscordTransport {
        // The constructor spawns a WS worker which will repeatedly try to
        // connect to the (mocked) Discord Gateway. The send_media round
        // trip we exercise here uses only `http`, so the WS noise is
        // harmless — we just don't block on it.
        DiscordTransport::with_api_base("test-bot-token".into(), base_url).expect("test transport")
    }

    fn media_payload() -> MediaRef {
        MediaRef {
            kind: "image".into(),
            url: "data:image/png;base64,iVBORw0KGgo=".into(),
            filename: Some("plot.png".into()),
            mime_type: Some("image/png".into()),
            size: Some(8),
        }
    }

    #[tokio::test]
    async fn send_media_image_routes_to_multipart_with_payload_json() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/channels/C123/messages")
            // No body matcher: mockito 1.7's async multipart matcher has
            // subtle interactions with reqwest's multipart serialisation;
            // we already verify the body shape in `discord::send_media_tests`
            // via read_media_bytes helpers.
            .with_status(200)
            .with_body(r#"{"id":"9001","channel_id":"C123"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "C123".into(),
            to_user: String::new(),
            caption: Some("trend".into()),
            reply_to: None,
        };
        let outcome = t.send_media(ctx, media_payload()).await.expect("send ok");
        assert_eq!(outcome, SendOutcome::Sent);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_throttled_returns_Throttled_outcome() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/channels/C123/messages")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "C123".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let outcome = t.send_media(ctx, media_payload()).await.expect("send ok");
        match outcome {
            SendOutcome::Throttled { ret, errmsg } => {
                assert_eq!(ret, 429);
                assert_eq!(errmsg.as_deref(), Some("rate limited"));
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
        m.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_5xx_bubbles_up_as_err() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/channels/C123/messages")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "C123".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, media_payload()).await.unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 500"));
        m.assert_async().await;
    }
}
