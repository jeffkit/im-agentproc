//! Telegram Bot API transport adapter.
//!
//! Uses long-polling (`getUpdates?timeout=30`) to receive messages and
//! `sendMessage` to reply. No WebSocket needed — plain HTTPS polling,
//! very similar to the iLink adapter.
//!
//! Configuration (`im_credentials:`):
//! - `token`: Telegram bot token from BotFather (e.g. `123456:ABCdef…`)
//!
//! The `buf` parameter stores the next polling offset as a decimal string.
//! On first call `buf` is empty and the offset defaults to 0.
//!
//! Media handling: photos, documents, audio, video and voice messages are
//! downloaded to temp files and forwarded as `MediaRef` attachments.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::media::download_to_temp;
use super::{
    InboundMessage, InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};

const BASE_URL: &str = "https://api.telegram.org";
/// Long-poll timeout in seconds (Telegram max is 50, keep headroom).
const POLL_TIMEOUT_SECS: u64 = 30;

// ── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<PhotoSize>,
    document: Option<Document>,
    audio: Option<Audio>,
    video: Option<Video>,
    voice: Option<Voice>,
    #[serde(default)]
    via_bot: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
struct PhotoSize {
    file_id: String,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Document {
    file_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Audio {
    file_id: String,
    mime_type: Option<String>,
    file_name: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Video {
    file_id: String,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Voice {
    file_id: String,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    username: Option<String>,
    first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
    first_name: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    is_bot: bool,
}

#[derive(Debug, Default, Deserialize)]
struct TgFile {
    file_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct GetUpdatesRequest {
    offset: i64,
    timeout: u64,
    allowed_updates: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
    parse_mode: Option<&'static str>,
}

// ── Transport implementation ─────────────────────────────────────────────────

/// Telegram Bot API transport (long-poll).
#[derive(Clone)]
pub struct TelegramTransport {
    http: reqwest::Client,
    token: String,
    base_url: String,
}

impl TelegramTransport {
    /// Create a new transport with the given bot token.
    pub fn new(token: String) -> Result<Self> {
        Self::with_base_url(token, BASE_URL.to_string())
    }

    /// Construct with an explicit API base url. Tests use this to point the
    /// transport at a local mockito server; production code should call
    /// [`Self::new`].
    pub fn with_base_url(token: String, base_url: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // Long-poll timeout + generous buffer
            .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 15))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client for Telegram")?;
        Ok(Self {
            http,
            token,
            base_url,
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    /// Resolve a `file_id` to a downloadable CDN URL via `getFile`.
    async fn get_file_url(&self, file_id: &str) -> Result<String> {
        let url = self.api_url(&format!("getFile?file_id={file_id}"));
        let resp: TgResponse<TgFile> = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("Telegram getFile HTTP")?
            .json()
            .await
            .context("Telegram getFile JSON")?;
        let file = resp.result.context("getFile: no result")?;
        let path = file.file_path.context("getFile: no file_path")?;
        Ok(format!("{BASE_URL}/file/bot{}/{path}", self.token))
    }

    /// Download a Telegram file by `file_id` and return a [`MediaRef`].
    async fn download_tg_file(
        &self,
        file_id: &str,
        kind: &str,
        filename: Option<&str>,
        mime_type: Option<&str>,
    ) -> Result<MediaRef> {
        let cdn_url = self.get_file_url(file_id).await?;
        // No auth header needed — Telegram CDN URLs are self-authenticating via the path.
        download_to_temp(&self.http, &cdn_url, None, kind, filename, mime_type).await
    }

    async fn get_updates(&self, offset: i64) -> Result<InboundOutcome> {
        let req = GetUpdatesRequest {
            offset,
            timeout: POLL_TIMEOUT_SECS,
            allowed_updates: vec!["message"],
        };
        let resp: TgResponse<Vec<Update>> = self
            .http
            .post(self.api_url("getUpdates"))
            .json(&req)
            .send()
            .await
            .context("Telegram getUpdates HTTP error")?
            .json()
            .await
            .context("Telegram getUpdates JSON parse")?;

        if !resp.ok {
            let code = resp.error_code.unwrap_or(0);
            let desc = resp.description.unwrap_or_default();
            // 401 Unauthorized → token rejected
            if code == 401 {
                return Ok(InboundOutcome::TokenRejected);
            }
            anyhow::bail!("Telegram getUpdates error {code}: {desc}");
        }

        let updates = resp.result.unwrap_or_default();
        let mut msgs: Vec<InboundMessage> = Vec::with_capacity(updates.len());
        for update in updates {
            if let Some(inbound) = self.tg_update_to_inbound(update).await {
                msgs.push(inbound);
            }
        }
        Ok(InboundOutcome::Messages(msgs))
    }

    async fn tg_update_to_inbound(&self, update: Update) -> Option<InboundMessage> {
        let msg = update.message?;

        let is_from_bot =
            msg.from.as_ref().map(|u| u.is_bot).unwrap_or(false) || msg.via_bot.is_some();
        if is_from_bot {
            warn!(
                msg_id = msg.message_id,
                "dropping Telegram message from bot (anti-loop)"
            );
            return None;
        }

        let text = msg
            .text
            .as_deref()
            .or(msg.caption.as_deref())
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string());

        // ── Download media attachments ──────────────────────────────────────
        let mut media: Vec<MediaRef> = vec![];

        if !msg.photo.is_empty() {
            // Telegram sends multiple sizes; pick the largest by file_size.
            if let Some(largest) = msg.photo.iter().max_by_key(|p| p.file_size.unwrap_or(0)) {
                match self
                    .download_tg_file(&largest.file_id, "image", None, Some("image/jpeg"))
                    .await
                {
                    Ok(r) => media.push(r),
                    Err(e) => warn!(error = %e, "Telegram photo download failed"),
                }
            }
        } else if let Some(doc) = msg.document.as_ref() {
            match self
                .download_tg_file(
                    &doc.file_id,
                    "file",
                    doc.file_name.as_deref(),
                    doc.mime_type.as_deref(),
                )
                .await
            {
                Ok(r) => media.push(r),
                Err(e) => warn!(error = %e, "Telegram document download failed"),
            }
        } else if let Some(audio) = msg.audio.as_ref() {
            match self
                .download_tg_file(
                    &audio.file_id,
                    "audio",
                    audio.file_name.as_deref(),
                    audio.mime_type.as_deref(),
                )
                .await
            {
                Ok(r) => media.push(r),
                Err(e) => warn!(error = %e, "Telegram audio download failed"),
            }
        } else if let Some(video) = msg.video.as_ref() {
            match self
                .download_tg_file(&video.file_id, "video", None, video.mime_type.as_deref())
                .await
            {
                Ok(r) => media.push(r),
                Err(e) => warn!(error = %e, "Telegram video download failed"),
            }
        } else if let Some(voice) = msg.voice.as_ref() {
            match self
                .download_tg_file(&voice.file_id, "audio", None, voice.mime_type.as_deref())
                .await
            {
                Ok(r) => media.push(r),
                Err(e) => warn!(error = %e, "Telegram voice download failed"),
            }
        }

        // Require text OR media; media-only messages get a placeholder description.
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

        let chat_id = msg.chat.id;
        let from_user = msg.from.as_ref().map(|u| u.id.to_string());
        let session_name = chat_name(&msg.chat);
        let extra = serde_json::json!({ "update_id": update.update_id });

        Some(InboundMessage {
            context_token: Some(chat_id.to_string()),
            from_user,
            is_from_bot,
            text,
            media,
            session_id: None,
            session_name,
            a2a_call_id: None,
            extra,
            raw: serde_json::json!({}),
        })
    }

    async fn send_message(&self, chat_id: i64, text: &str) -> Result<SendOutcome> {
        let req = SendMessageRequest {
            chat_id,
            text: text.to_string(),
            parse_mode: None,
        };
        let resp: TgResponse<serde_json::Value> = self
            .http
            .post(self.api_url("sendMessage"))
            .json(&req)
            .send()
            .await
            .context("Telegram sendMessage HTTP error")?
            .json()
            .await
            .context("Telegram sendMessage JSON parse")?;

        if !resp.ok {
            let code = resp.error_code.unwrap_or(0);
            let desc = resp.description.as_deref().unwrap_or("");
            // 429 Too Many Requests → throttle
            if code == 429 {
                return Ok(SendOutcome::Throttled {
                    ret: 429,
                    errmsg: Some(desc.to_string()),
                });
            }
            anyhow::bail!("Telegram sendMessage error {code}: {desc}");
        }
        Ok(SendOutcome::Sent)
    }
}

impl Transport for TelegramTransport {
    fn next_inbound<'a>(&'a self, buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            // `buf` stores the next offset as a decimal string.
            let offset: i64 = buf.trim().parse().unwrap_or(0);
            let outcome = self.get_updates(offset).await?;

            // Advance the offset past the highest update_id we've seen.
            if let InboundOutcome::Messages(ref msgs) = outcome {
                for msg in msgs {
                    if let Some(ref extra) = msg.extra.get("update_id") {
                        if let Some(uid) = extra.as_i64() {
                            let next = uid + 1;
                            let cur: i64 = buf.trim().parse().unwrap_or(0);
                            if next > cur {
                                *buf = next.to_string();
                            }
                        }
                    }
                }
                debug!(count = msgs.len(), next_offset = %buf, "Telegram getUpdates");
            }
            Ok(outcome)
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        Box::pin(async move {
            // context_token is the chat_id as a decimal string
            let chat_id: i64 = reply
                .context_token
                .trim()
                .parse()
                .context("Telegram context_token must be a chat_id integer")?;
            if reply.text.trim().is_empty() {
                return Ok(SendOutcome::Sent);
            }
            self.send_message(chat_id, &reply.text).await
        })
    }

    fn name(&self) -> &'static str {
        "telegram"
    }

    fn send_media<'a>(
        &'a self,
        ctx: MediaOut,
        media: MediaRef,
    ) -> BoxFuture<'a, Result<SendOutcome>> {
        let http = self.http.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let chat_id: i64 = ctx
                .context_token
                .trim()
                .parse()
                .context("Telegram context_token must be a chat_id integer")?;
            let bytes = super::media::read_media_bytes(&http, &media).await?;
            let filename = media
                .filename
                .clone()
                .or_else(|| super::media::filename_from_url(&media.url))
                .unwrap_or_else(|| "attachment".to_string());
            let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.clone());
            let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string());
            // Map kind → Telegram method. Images get sendPhoto (auto-preview);
            // everything else goes via sendDocument for fidelity. `voice` is
            // not modelled as a Telegram voice-note here; we send as audio.
            let (method, field_name) = match media.kind.as_str() {
                "image" => ("sendPhoto", "photo"),
                _ => ("sendDocument", "document"),
            };
            form = form.text("caption", ctx.caption.clone().unwrap_or_default());
            form = form.part(field_name, part);
            let resp = self
                .http
                .post(self.api_url(method))
                .bearer_auth(&token)
                .multipart(form)
                .send()
                .await
                .context("Telegram sendMedia HTTP")?;
            let status = resp.status();
            if status.as_u16() == 429 {
                let body = resp.text().await.unwrap_or_default();
                return Ok(SendOutcome::Throttled {
                    ret: 429,
                    errmsg: Some(body),
                });
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram sendMedia HTTP {status}: {body}");
            }
            let resp: TgResponse<serde_json::Value> =
                resp.json().await.context("Telegram sendMedia JSON")?;
            if !resp.ok {
                let code = resp.error_code.unwrap_or(0);
                let desc = resp.description.as_deref().unwrap_or("").to_string();
                if code == 429 {
                    return Ok(SendOutcome::Throttled {
                        ret: 429,
                        errmsg: Some(desc),
                    });
                }
                anyhow::bail!("Telegram {method} error {code}: {desc}");
            }
            Ok(SendOutcome::Sent)
        })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities { media_upload: true }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn chat_name(chat: &Chat) -> Option<String> {
    chat.title
        .as_deref()
        .or(chat.username.as_deref())
        .or(chat.first_name.as_deref())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod send_media_e2e_tests {
    use super::*;
    use crate::bridge::transport::SendOutcome;

    /// Build a TelegramTransport pointing at `base_url` (typically a mockito
    /// server) so the send_media round trip is fully hermetic.
    fn transport_for(base_url: String) -> TelegramTransport {
        TelegramTransport::with_base_url("123:abc".into(), base_url).expect("test transport")
    }

    fn png_media(filename: &str) -> MediaRef {
        MediaRef {
            kind: "image".into(),
            url: format!("data:image/png;base64,iVBORw0KGgo="), // 8-byte stub PNG
            filename: Some(filename.into()),
            mime_type: Some("image/png".into()),
            size: Some(8),
        }
    }

    #[tokio::test]
    async fn send_media_image_routes_to_sendPhoto_multipart() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/bot123:abc/sendPhoto")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("chat_id".into()),
                mockito::Matcher::Regex("caption".into()),
                mockito::Matcher::Regex("photo".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "123".into(),
            to_user: String::new(),
            caption: Some("trend".into()),
            reply_to: None,
        };
        let outcome = t
            .send_media(ctx, png_media("plot.png"))
            .await
            .expect("sendPhoto ok");
        assert_eq!(outcome, SendOutcome::Sent);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_throttled_returns_Throttled_outcome() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/bot123:abc/sendDocument")
            .with_status(429)
            .with_body(r#"{"ok":false,"error_code":429,"description":"slow down"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "123".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let media = MediaRef {
            kind: "file".into(),
            url: "data:application/pdf;base64,JVBERi0=".into(),
            filename: Some("x.pdf".into()),
            mime_type: Some("application/pdf".into()),
            size: Some(8),
        };
        let outcome = t
            .send_media(ctx, media)
            .await
            .expect("sendDocument throttled");
        match outcome {
            SendOutcome::Throttled { ret, errmsg } => {
                assert_eq!(ret, 429);
                assert!(errmsg.unwrap_or_default().contains("slow down"));
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
        m.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_invalid_chat_id_returns_err() {
        let server = mockito::Server::new_async().await;
        // No mock needed — Telegram parses chat_id from context_token before
        // hitting the wire. An invalid id surfaces as Err.
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "not-a-number".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, png_media("x.png")).await.unwrap_err();
        assert!(format!("{err:#}").contains("chat_id"));
    }

    #[tokio::test]
    async fn send_media_5xx_bubbles_up_as_err() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/bot123:abc/sendPhoto")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "123".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, png_media("x.png")).await.unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 500"));
        m.assert_async().await;
    }
}
