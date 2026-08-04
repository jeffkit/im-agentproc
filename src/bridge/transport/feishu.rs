//! 飞书（Lark）WebSocket 长连接 Transport 适配器。
//!
//! 使用 `larksuite-oapi-sdk-rs` 建立飞书 WebSocket 长连接，订阅
//! `im.message.receive_v1` 事件；通过飞书 IM HTTP API 回复消息。
//! 无需公网 URL，适用于自建企业应用。
//!
//! 配置 (`im_credentials:`):
//! - `app_id`:     飞书应用的 App ID
//! - `app_secret`: 飞书应用的 App Secret
//!
//! 飞书开发者文档：
//! <https://open.feishu.cn/document/ukTMukTMukTM/uYDNxYjL2QTM24iN0EjN/event-subscription-configure->

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use super::media::download_to_temp;
use super::{
    InboundMessage, InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};

const FEISHU_API: &str = "https://open.feishu.cn/open-apis";
/// Refresh the tenant_access_token 5 minutes before it expires.
const TOKEN_REFRESH_BUFFER_SECS: u64 = 300;

// ── Token cache ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TokenCache {
    token: String,
    created_at: Instant,
    expire_secs: u64,
}

impl TokenCache {
    fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() + TOKEN_REFRESH_BUFFER_SECS < self.expire_secs
    }
}

// ── Wire types (Feishu event JSON) ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TenantTokenResp {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    tenant_access_token: Option<String>,
    #[serde(default)]
    expire: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SendMsgRequest<'a> {
    receive_id: &'a str,
    msg_type: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct SendMsgResp {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FeishuUploadData {
    #[serde(default)]
    file_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FeishuUploadResponse {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<FeishuUploadData>,
}

// ── Transport ────────────────────────────────────────────────────────────────

/// 飞书 WebSocket 长连接 Transport。
pub struct FeishuTransport {
    inbound_rx: Mutex<mpsc::UnboundedReceiver<InboundOutcome>>,
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    token_cache: Arc<Mutex<Option<TokenCache>>>,
    /// API base url. Defaults to `https://open.feishu.cn/open-apis`; tests
    /// override to point at a mockito server.
    api_base: String,
}

impl FeishuTransport {
    /// 创建 Transport 并在后台启动飞书 WebSocket 长连接。
    pub fn new(app_id: String, app_secret: String) -> Result<Self> {
        Self::with_api_base(app_id, app_secret, FEISHU_API.to_string())
    }

    /// Construct with an explicit API base url. Tests use this to point the
    /// transport at a local mockito server; production code should call
    /// [`Self::new`].
    pub fn with_api_base(app_id: String, app_secret: String, api_base: String) -> Result<Self> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<InboundOutcome>();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client for Feishu")?;

        // Build the larksuite SDK client for WebSocket events.
        let sdk_client = larksuite_oapi_sdk_rs::LarkClient::builder(&app_id, &app_secret)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build Feishu SDK client: {e:?}"))?;

        let tx = inbound_tx.clone();
        let http_for_handler = http.clone();
        let app_id_for_handler = app_id.clone();
        let app_secret_for_handler = app_secret.clone();
        let api_base_for_handler = api_base.clone();
        let token_cache_for_handler: Arc<Mutex<Option<TokenCache>>> = Arc::new(Mutex::new(None));
        let token_cache_ref = token_cache_for_handler.clone();

        // Register event handler for inbound messages.
        // on_event receives larksuite_oapi_sdk_rs::JsonValue (transparent wrapper around
        // serde_json::Value). The closure must return Result<(), LarkError>.
        let dispatcher = larksuite_oapi_sdk_rs::EventDispatcher::new("", "").on_event(
            "im.message.receive_v1",
            move |event: larksuite_oapi_sdk_rs::JsonValue| {
                let tx = tx.clone();
                let http = http_for_handler.clone();
                let app_id = app_id_for_handler.clone();
                let app_secret = app_secret_for_handler.clone();
                let api_base = api_base_for_handler.clone();
                let token_cache = token_cache_ref.clone();
                async move {
                    if let Some(msg) = feishu_event_to_inbound(
                        event.as_value(),
                        &http,
                        &app_id,
                        &app_secret,
                        &api_base,
                        &token_cache,
                    )
                    .await
                    {
                        let _ = tx.send(InboundOutcome::Messages(vec![msg]));
                    }
                    Ok::<(), larksuite_oapi_sdk_rs::LarkError>(())
                }
            },
        );

        // Spawn background WS worker. WsClient has auto_reconnect=true by default,
        // so a single start() call handles reconnections internally.
        tokio::spawn(async move {
            info!("Feishu WS: starting (auto_reconnect enabled)…");
            if let Err(e) = sdk_client.ws_client(dispatcher).start().await {
                error!(error = %e, "Feishu WS fatal error; no further reconnects");
            }
        });

        Ok(Self {
            inbound_rx: Mutex::new(inbound_rx),
            http,
            app_id,
            app_secret,
            // Share token_cache with the event handler so both use the same cached token.
            token_cache: token_cache_for_handler,
            api_base,
        })
    }

    async fn get_token(&self) -> Result<String> {
        feishu_get_token(
            &self.http,
            &self.app_id,
            &self.app_secret,
            &self.api_base,
            &self.token_cache,
        )
        .await
    }
}

impl Transport for FeishuTransport {
    fn next_inbound<'a>(&'a self, _buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            let mut rx = self.inbound_rx.lock().await;
            rx.recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("Feishu inbound channel closed"))
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        Box::pin(async move {
            if reply.text.trim().is_empty() {
                return Ok(SendOutcome::Sent);
            }
            let token = self.get_token().await?;
            // context_token = chat_id
            let chat_id = &reply.context_token;
            let content = serde_json::json!({"text": reply.text}).to_string();
            let req = SendMsgRequest {
                receive_id: chat_id,
                msg_type: "text",
                content,
            };
            let resp: SendMsgResp = self
                .http
                .post(format!(
                    "{}/im/v1/messages?receive_id_type=chat_id",
                    self.api_base
                ))
                .bearer_auth(&token)
                .json(&req)
                .send()
                .await
                .context("Feishu sendMessage HTTP")?
                .json()
                .await
                .context("Feishu sendMessage JSON")?;
            if resp.code != 0 {
                // 99991400 = message too long; treat as throttle
                if resp.code == 99991400 {
                    return Ok(SendOutcome::Throttled {
                        ret: resp.code as i32,
                        errmsg: resp.msg,
                    });
                }
                anyhow::bail!(
                    "Feishu sendMessage failed (code={}): {}",
                    resp.code,
                    resp.msg.unwrap_or_default()
                );
            }
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "feishu"
    }

    fn send_media<'a>(
        &'a self,
        ctx: MediaOut,
        media: MediaRef,
    ) -> BoxFuture<'a, Result<SendOutcome>> {
        let http = self.http.clone();
        let app_id = self.app_id.clone();
        let app_secret = self.app_secret.clone();
        let token_cache = self.token_cache.clone();
        Box::pin(async move {
            let token =
                feishu_get_token(&http, &app_id, &app_secret, &self.api_base, &token_cache).await?;
            let receive_id = &ctx.context_token;
            let bytes = super::media::read_media_bytes(&http, &media).await?;
            // Feishu splits image / file / video into distinct upload endpoints.
            // Map our `kind` to the right resource type and call the upload.
            let (resource_type, upload_path) = match media.kind.as_str() {
                "image" => ("image", "im/v1/images"),
                "video" => ("video", "im/v1/files"),
                _ => ("file", "im/v1/files"),
            };
            let part = reqwest::multipart::Part::bytes(bytes).file_name(
                media
                    .filename
                    .clone()
                    .unwrap_or_else(|| "attachment".into()),
            );
            let form = reqwest::multipart::Form::new()
                .text("resource_type", resource_type.to_string())
                .part("file", part);
            let upload: FeishuUploadResponse = http
                .post(format!("{}/{}", self.api_base, upload_path))
                .bearer_auth(&token)
                .multipart(form)
                .send()
                .await
                .context("Feishu upload HTTP")?
                .json()
                .await
                .context("Feishu upload JSON")?;
            if upload.code != 0 {
                anyhow::bail!(
                    "Feishu upload failed (code={}): {}",
                    upload.code,
                    upload.msg.unwrap_or_default()
                );
            }
            let file_key = upload
                .data
                .as_ref()
                .and_then(|d| d.file_key.clone())
                .ok_or_else(|| anyhow::anyhow!("Feishu upload returned no file_key"))?;
            // Send a message referencing the uploaded media.
            let msg_type = match media.kind.as_str() {
                "image" => "image",
                "video" => "media",
                _ => "file",
            };
            let content = match msg_type {
                "image" => serde_json::json!({"image_key": file_key}),
                "media" => serde_json::json!({
                    "video_key": file_key,
                    "cover_image_key": file_key, // best-effort; real covers uploaded separately
                }),
                _ => serde_json::json!({"file_key": file_key}),
            }
            .to_string();
            let req = SendMsgRequest {
                receive_id,
                msg_type,
                content,
            };
            let resp: SendMsgResp = http
                .post(format!(
                    "{}/im/v1/messages?receive_id_type=chat_id",
                    self.api_base
                ))
                .bearer_auth(&token)
                .json(&req)
                .send()
                .await
                .context("Feishu sendMedia HTTP")?
                .json()
                .await
                .context("Feishu sendMedia JSON")?;
            if resp.code != 0 {
                if resp.code == 99991400 {
                    return Ok(SendOutcome::Throttled {
                        ret: resp.code as i32,
                        errmsg: resp.msg,
                    });
                }
                anyhow::bail!(
                    "Feishu sendMedia failed (code={}): {}",
                    resp.code,
                    resp.msg.unwrap_or_default()
                );
            }
            let _ = ctx.caption; // caption is not separately supported on Feishu image msgs
            Ok(SendOutcome::Sent)
        })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities { media_upload: true }
    }
}

// ── Conversion helpers ────────────────────────────────────────────────────────

async fn feishu_event_to_inbound(
    event: &serde_json::Value,
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    api_base: &str,
    token_cache: &Arc<Mutex<Option<TokenCache>>>,
) -> Option<InboundMessage> {
    // The `on_event` handler receives the full event body.
    // Navigate to the inner `event` field if present.
    let inner = event.get("event").unwrap_or(event);

    let sender = inner.get("sender")?;
    let msg = inner.get("message")?;

    // Check sender type first for anti-loop
    let is_from_bot = sender.get("sender_type").and_then(|t| t.as_str()) == Some("bot");
    if is_from_bot {
        warn!("Feishu: dropping message from bot (anti-loop)");
        return None;
    }

    let msg_type = msg.get("msg_type")?.as_str()?;
    let message_id = msg.get("message_id")?.as_str()?;

    let content_str = msg.get("content")?.as_str().unwrap_or("{}");
    let content: serde_json::Value = serde_json::from_str(content_str).unwrap_or_default();

    let mut text: Option<String> = None;
    let mut media: Vec<MediaRef> = vec![];

    match msg_type {
        "text" => {
            text = content
                .get("text")
                .and_then(|t| t.as_str())
                .filter(|t| !t.trim().is_empty())
                .map(|s| s.to_string());
        }
        "image" => {
            if let Some(image_key) = content.get("image_key").and_then(|k| k.as_str()) {
                match download_feishu_resource(
                    http,
                    app_id,
                    app_secret,
                    api_base,
                    token_cache,
                    message_id,
                    image_key,
                    "image",
                    "image",
                    None,
                    Some("image/jpeg"),
                )
                .await
                {
                    Ok(r) => media.push(r),
                    Err(e) => warn!(error = %e, "Feishu image download failed"),
                }
            }
        }
        "file" => {
            let file_key = content.get("file_key").and_then(|k| k.as_str());
            let filename = content.get("file_name").and_then(|n| n.as_str());
            if let Some(key) = file_key {
                match download_feishu_resource(
                    http,
                    app_id,
                    app_secret,
                    api_base,
                    token_cache,
                    message_id,
                    key,
                    "file",
                    "file",
                    filename,
                    None,
                )
                .await
                {
                    Ok(r) => media.push(r),
                    Err(e) => warn!(error = %e, "Feishu file download failed"),
                }
            }
        }
        "audio" => {
            if let Some(file_key) = content.get("file_key").and_then(|k| k.as_str()) {
                match download_feishu_resource(
                    http,
                    app_id,
                    app_secret,
                    api_base,
                    token_cache,
                    message_id,
                    file_key,
                    "file",
                    "audio",
                    None,
                    Some("audio/ogg"),
                )
                .await
                {
                    Ok(r) => media.push(r),
                    Err(e) => warn!(error = %e, "Feishu audio download failed"),
                }
            }
        }
        other => {
            debug!(
                msg_type = other,
                "Feishu: ignoring unsupported message type"
            );
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

    let chat_id = msg
        .get("chat_id")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let chat_type = msg
        .get("chat_type")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let from_user = sender
        .get("sender_id")
        .and_then(|id| id.get("user_id"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());

    let session_name = chat_type.as_deref().map(|t| format!("feishu-{t}"));

    Some(InboundMessage {
        context_token: chat_id.clone(),
        from_user,
        is_from_bot: false,
        text,
        media,
        session_id: chat_id,
        session_name,
        a2a_call_id: None,
        extra: inner.clone(),
        raw: event.clone(),
    })
}

/// Download a Feishu message resource (image/file/audio) using the message resource API.
/// `resource_type` is the Feishu API type string: `"image"` or `"file"`.
#[allow(clippy::too_many_arguments)]
async fn download_feishu_resource(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    api_base: &str,
    token_cache: &Arc<Mutex<Option<TokenCache>>>,
    message_id: &str,
    file_key: &str,
    resource_type: &str,
    kind: &str,
    filename: Option<&str>,
    mime_type: Option<&str>,
) -> Result<MediaRef> {
    // Obtain a fresh access token.
    let token = feishu_get_token(http, app_id, app_secret, api_base, token_cache).await?;
    let url = format!(
        "{}/im/v1/messages/{message_id}/resources/{file_key}?type={resource_type}",
        api_base
    );
    download_to_temp(
        http,
        &url,
        Some(&format!("Bearer {token}")),
        kind,
        filename,
        mime_type,
    )
    .await
}

/// Fetch or refresh the Feishu tenant_access_token (standalone, shareable).
async fn feishu_get_token(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
    api_base: &str,
    token_cache: &Arc<Mutex<Option<TokenCache>>>,
) -> Result<String> {
    let mut cache = token_cache.lock().await;
    if let Some(ref c) = *cache {
        if c.is_valid() {
            return Ok(c.token.clone());
        }
    }
    debug!("Feishu: refreshing tenant_access_token");
    let resp: TenantTokenResp = http
        .post(format!("{api_base}/auth/v3/tenant_access_token/internal"))
        .json(&serde_json::json!({"app_id": app_id, "app_secret": app_secret}))
        .send()
        .await
        .context("Feishu tenant_access_token HTTP")?
        .json()
        .await
        .context("Feishu tenant_access_token JSON")?;
    if resp.code != 0 {
        anyhow::bail!(
            "Feishu tenant_access_token failed (code={}): {}",
            resp.code,
            resp.msg.unwrap_or_default()
        );
    }
    let token = resp
        .tenant_access_token
        .context("missing tenant_access_token")?;
    let expire = resp.expire.unwrap_or(7200);
    *cache = Some(TokenCache {
        token: token.clone(),
        created_at: Instant::now(),
        expire_secs: expire,
    });
    Ok(token)
}

#[cfg(test)]
mod send_media_e2e_tests {
    use super::*;

    fn transport_for(api_base: String) -> FeishuTransport {
        // The constructor spawns a WS worker that tries to connect to the
        // Feishu WebSocket gateway. We don't gate tests on it; the upload
        // path uses `http` only.
        FeishuTransport::with_api_base("cli_test".into(), "secret_test".into(), api_base)
            .expect("test transport")
    }

    fn png_payload() -> MediaRef {
        MediaRef {
            kind: "image".into(),
            url: "data:image/png;base64,iVBORw0KGgo=".into(),
            filename: Some("plot.png".into()),
            mime_type: Some("image/png".into()),
            size: Some(8),
        }
    }

    #[tokio::test]
    async fn send_media_image_uploads_then_references_file_key() {
        let mut server = mockito::Server::new_async().await;
        // Step 1: tenant_access_token
        let m_token = server
            .mock("POST", "/auth/v3/tenant_access_token/internal")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok","tenant_access_token":"t-1","expire":7200}"#)
            .create_async()
            .await;
        // Step 2: image upload
        let m_upload = server
            .mock("POST", "/im/v1/images")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok","data":{"file_key":"img_key_42"}}"#)
            .create_async()
            .await;
        // Step 3: send message
        let m_send = server
            .mock("POST", "/im/v1/messages?receive_id_type=chat_id")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "oc_xxx".into(),
            to_user: String::new(),
            caption: Some("trend".into()),
            reply_to: None,
        };
        let outcome = t.send_media(ctx, png_payload()).await.expect("send ok");
        assert_eq!(outcome, SendOutcome::Sent);
        m_token.assert_async().await;
        m_upload.assert_async().await;
        m_send.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_throttled_returns_Throttled_outcome() {
        let mut server = mockito::Server::new_async().await;
        let _ = server
            .mock("POST", "/auth/v3/tenant_access_token/internal")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok","tenant_access_token":"t-1","expire":7200}"#)
            .create_async()
            .await;
        let _ = server
            .mock("POST", "/im/v1/images")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok","data":{"file_key":"img_key_42"}}"#)
            .create_async()
            .await;
        let m_send = server
            .mock("POST", "/im/v1/messages?receive_id_type=chat_id")
            .with_status(200)
            .with_body(r#"{"code":99991400,"msg":"too long"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "oc_xxx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let outcome = t.send_media(ctx, png_payload()).await.expect("send ok");
        match outcome {
            SendOutcome::Throttled { ret, errmsg } => {
                assert_eq!(ret, 99991400);
                assert!(errmsg.unwrap_or_default().contains("too long"));
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
        m_send.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_upload_failure_bubbles_up_as_err() {
        let mut server = mockito::Server::new_async().await;
        let _ = server
            .mock("POST", "/auth/v3/tenant_access_token/internal")
            .with_status(200)
            .with_body(r#"{"code":0,"msg":"ok","tenant_access_token":"t-1","expire":7200}"#)
            .create_async()
            .await;
        let m_upload = server
            .mock("POST", "/im/v1/images")
            .with_status(200)
            .with_body(r#"{"code":230001,"msg":"upload failed"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "oc_xxx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, png_payload()).await.unwrap_err();
        assert!(format!("{err:#}").contains("upload failed"));
        m_upload.assert_async().await;
    }
}
