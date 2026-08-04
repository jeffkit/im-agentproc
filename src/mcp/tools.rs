//! MCP tool registry and handlers.
//!
//! Each outbound tool is a struct implementing [`ToolHandler`] — exposes a
//! [`ToolDescriptor`] (name + JSON Schema for `arguments`) plus an async
//! `call` method that drives the underlying [`Transport`]. The
//! [`ToolRegistry`] is a name → handler map; [`crate::mcp::run_server`] queries
//! it for `tools/list` and `tools/call`.
//!
//! The four handlers (`send_text`, `send_image`, `send_file`, `send_voice`)
//! share a common shape: validate the supplied `source.uri` + dispatch to
//! `Transport::send_reply` (for `send_text`) or `Transport::send_media` (for
//! the others). A single [`OutboundDelivery`] helper bundles the routing +
//! media-ref construction so each tool stays small.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::bridge::transport::{MediaOut, MediaRef, OutboundReply, SendOutcome, Transport};

/// Tool descriptor — surfaced via `tools/list`. The `input_schema` is a
/// JSON Schema object per the MCP 2025-06-18 spec.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: Value,
}

/// Async trait implemented by every tool handler.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    async fn call(&self, args: Value) -> Result<Value>;
}

/// Name → handler map.
#[derive(Default)]
pub struct ToolRegistry {
    by_name: HashMap<String, Arc<dyn ToolHandler>>,
}

impl ToolRegistry {
    pub fn register(&mut self, handler: Arc<dyn ToolHandler>) {
        let name = handler.descriptor().name.clone();
        self.by_name.insert(name, handler);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.by_name.get(name).cloned()
    }

    pub fn list(&self) -> Vec<ToolDescriptor> {
        let mut out: Vec<ToolDescriptor> = self.by_name.values().map(|h| h.descriptor()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

// ── Common delivery helper ────────────────────────────────────────────────────

/// Shared state for the four delivery tools: the underlying transport plus
/// the inbound conversation context the bridge handed us.
pub struct OutboundDelivery {
    pub transport: Arc<dyn Transport>,
    /// `context_token` from the inbound message (chat_id / channel_id /
    /// req_id). The MCP server is per-bridge-child, so this is set once at
    /// bridge startup and reused on every tool call.
    pub context_token: String,
    /// Optional `to_user` override; defaults to the inbound `from_user` when
    /// the bridge has one. The MCP server fills this in at bridge startup.
    pub to_user: String,
}

impl OutboundDelivery {
    pub fn new(transport: Arc<dyn Transport>, context_token: String, to_user: String) -> Self {
        Self {
            transport,
            context_token,
            to_user,
        }
    }

    fn media_out(&self, caption: Option<String>, reply_to: Option<String>) -> MediaOut {
        MediaOut {
            context_token: self.context_token.clone(),
            to_user: self.to_user.clone(),
            caption,
            reply_to,
        }
    }

    /// Drive `Transport::send_media`. Returns the silent-success / loud-
    /// failure envelope already in MCP shape.
    pub async fn send(
        &self,
        media: MediaRef,
        caption: Option<String>,
        reply_to: Option<String>,
    ) -> Value {
        // Capabilities gate: refuse to silently no-op when the IM doesn't
        // support media upload — surface as loud failure so the agent
        // understands the call did nothing.
        if !self.transport.capabilities().media_upload {
            return crate::mcp::server::failure_text(format!(
                "transport `{}` does not support media upload",
                self.transport.name()
            ));
        }
        let ctx = self.media_out(caption, reply_to);
        match self.transport.send_media(ctx, media).await {
            Ok(SendOutcome::Sent) => crate::mcp::server::success_empty(),
            Ok(SendOutcome::Throttled { ret, errmsg }) => crate::mcp::server::failure_text(
                errmsg.unwrap_or_else(|| format!("throttled (code={ret})")),
            ),
            Err(err) => crate::mcp::server::failure_text(format!("{err:#}")),
        }
    }

    pub async fn send_text(&self, text: String, reply_to: Option<String>) -> Value {
        // Reuse `Transport::send_reply` so the transport-specific outbound
        // path (Telegram HTML fallback, Feishu 99991400 throttle mapping, …)
        // applies uniformly.
        let reply = OutboundReply {
            context_token: self.context_token.clone(),
            text,
            to_user: self.to_user.clone(),
            cli_session_id: None,
            session_name: None,
            a2a_call_id: None,
            usage: None,
        };
        let _ = reply_to; // reply_to not yet plumbed through OutboundReply; reserved for future use.
        match self.transport.send_reply(reply).await {
            Ok(SendOutcome::Sent) => crate::mcp::server::success_empty(),
            Ok(SendOutcome::Throttled { ret, errmsg }) => crate::mcp::server::failure_text(
                errmsg.unwrap_or_else(|| format!("throttled (code={ret})")),
            ),
            Err(err) => crate::mcp::server::failure_text(format!("{err:#}")),
        }
    }
}

// ── Tool: send_text ──────────────────────────────────────────────────────────

pub struct SendTextTool {
    pub delivery: Arc<OutboundDelivery>,
}

#[async_trait]
impl ToolHandler for SendTextTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "send_text".into(),
            title: "Send text".into(),
            description: "Send a text message to the current IM conversation. \
                          The text is rendered as-is on the IM platform. \
                          Use `reply_to` to quote a previous message id when the \
                          IM supports it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text body. May contain newlines, code blocks, markdown (rendered per IM convention).",
                    },
                    "reply_to": {
                        "type": "string",
                        "description": "Optional IM message id to quote-reply to. Ignored by IMs that don't support quoting.",
                    },
                },
                "required": ["text"],
            }),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .context("send_text: `text` is required and must be a string")?
            .to_string();
        if text.trim().is_empty() {
            // Empty text on a text send is a no-op — return silent success so
            // the agent doesn't have to special-case it.
            return Ok(crate::mcp::server::success_empty());
        }
        let reply_to = args
            .get("reply_to")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(self.delivery.send_text(text, reply_to).await)
    }
}

// ── Tool: send_image ─────────────────────────────────────────────────────────

pub struct SendImageTool {
    pub delivery: Arc<OutboundDelivery>,
}

#[async_trait]
impl ToolHandler for SendImageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "send_image".into(),
            title: "Send image".into(),
            description: "Send an image to the current IM conversation. \
                          The bridge accepts a `file://`, `https://`, or `data:` URI; \
                          remote URLs are downloaded server-side and uploaded to the IM."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "object",
                        "description": "Where to read the image from.",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "file://, https://, or data:image/...;base64,... URL.",
                            },
                            "name": {
                                "type": "string",
                                "description": "Original filename for display; inferred from URI when omitted.",
                            },
                        },
                        "required": ["uri"],
                    },
                    "caption": {
                        "type": "string",
                        "description": "Optional caption shown under the image.",
                    },
                    "reply_to": {
                        "type": "string",
                        "description": "Optional IM message id to quote-reply to.",
                    },
                    "as_document": {
                        "type": "boolean",
                        "description": "If true, send as uncompressed document instead of preview-compressed image. Default false.",
                    },
                },
                "required": ["source"],
            }),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let media = parse_source(&args, "image").context("send_image: invalid `source`")?;
        let caption = args
            .get("caption")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reply_to = args
            .get("reply_to")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(self.delivery.send(media, caption, reply_to).await)
    }
}

// ── Tool: send_file ──────────────────────────────────────────────────────────

pub struct SendFileTool {
    pub delivery: Arc<OutboundDelivery>,
}

#[async_trait]
impl ToolHandler for SendFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "send_file".into(),
            title: "Send file".into(),
            description: "Send a generic file (PDF / docx / zip / any MIME) to the \
                          current IM conversation. Same URI scheme rules as \
                          `send_image`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "object",
                        "description": "Where to read the file from.",
                        "properties": {
                            "uri": {"type": "string"},
                            "name": {"type": "string"},
                        },
                        "required": ["uri"],
                    },
                    "caption": {"type": "string"},
                    "reply_to": {"type": "string"},
                },
                "required": ["source"],
            }),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let media = parse_source(&args, "file").context("send_file: invalid `source`")?;
        let caption = args
            .get("caption")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reply_to = args
            .get("reply_to")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(self.delivery.send(media, caption, reply_to).await)
    }
}

// ── Tool: send_voice ─────────────────────────────────────────────────────────

pub struct SendVoiceTool {
    pub delivery: Arc<OutboundDelivery>,
}

#[async_trait]
impl ToolHandler for SendVoiceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "send_voice".into(),
            title: "Send voice".into(),
            description: "Send a voice / audio clip to the current IM conversation. \
                          IMs that distinguish voice from generic audio will route \
                          this to the voice-message slot."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "object",
                        "properties": {
                            "uri": {"type": "string"},
                            "name": {"type": "string"},
                        },
                        "required": ["uri"],
                    },
                    "caption": {"type": "string"},
                    "reply_to": {"type": "string"},
                },
                "required": ["source"],
            }),
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let media = parse_source(&args, "audio").context("send_voice: invalid `source`")?;
        let caption = args
            .get("caption")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reply_to = args
            .get("reply_to")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(self.delivery.send(media, caption, reply_to).await)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parse the `source: { uri, name? }` argument common to all three media
/// tools and build a [`MediaRef`]. Centralised so each tool stays small and
/// the validation rules (`uri` required, optional `name`) are uniform.
fn parse_source(args: &Value, kind: &str) -> Result<MediaRef> {
    let source = args
        .get("source")
        .and_then(Value::as_object)
        .context("`source` object is required")?;
    let uri = source
        .get("uri")
        .and_then(Value::as_str)
        .context("`source.uri` is required and must be a string")?;
    let name = source
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(MediaRef {
        kind: kind.to_string(),
        url: uri.to_string(),
        filename: name,
        mime_type: None,
        size: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::transport::{
        InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
        TransportCapabilities,
    };
    use futures_util::future::BoxFuture;
    use std::sync::Mutex;

    /// Test transport that records every send_reply/send_media invocation.
    struct CapturingTransport {
        text: Arc<Mutex<Vec<String>>>,
        media: Arc<Mutex<Vec<(MediaRef, MediaOut)>>>,
        name: &'static str,
        capabilities: TransportCapabilities,
    }

    impl CapturingTransport {
        fn new(name: &'static str, media_upload: bool) -> Self {
            Self {
                text: Arc::new(Mutex::new(Vec::new())),
                media: Arc::new(Mutex::new(Vec::new())),
                name,
                capabilities: TransportCapabilities { media_upload },
            }
        }
    }

    impl Transport for CapturingTransport {
        fn next_inbound<'a>(
            &'a self,
            _buf: &'a mut String,
        ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
            Box::pin(async move { Ok(InboundOutcome::Messages(vec![])) })
        }
        fn send_reply<'a>(
            &'a self,
            reply: OutboundReply,
        ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
            let text = self.text.clone();
            Box::pin(async move {
                text.lock().unwrap().push(reply.text);
                Ok(SendOutcome::Sent)
            })
        }
        fn send_media<'a>(
            &'a self,
            ctx: MediaOut,
            media: MediaRef,
        ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
            let bucket = self.media.clone();
            Box::pin(async move {
                bucket.lock().unwrap().push((media, ctx));
                Ok(SendOutcome::Sent)
            })
        }
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> TransportCapabilities {
            self.capabilities.clone()
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn send_text_dispatches_to_transport_send_reply() {
        let tr = Arc::new(CapturingTransport::new("telegram", false));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendTextTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool
                .call(json!({"text": "hello world"}))
                .await
                .expect("call");
            assert_eq!(v["isError"], false);
            assert_eq!(v["content"].as_array().unwrap().len(), 0);
        });
        assert_eq!(*tr.text.lock().unwrap(), vec!["hello world".to_string()]);
    }

    #[test]
    fn send_text_empty_text_is_silent_success() {
        let tr = Arc::new(CapturingTransport::new("telegram", false));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendTextTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool.call(json!({"text": "   "})).await.expect("call");
            assert_eq!(v["isError"], false);
        });
        assert!(tr.text.lock().unwrap().is_empty());
    }

    #[test]
    fn send_text_missing_text_returns_loud_failure() {
        let tr = Arc::new(CapturingTransport::new("telegram", false));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendTextTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let err = tool.call(json!({})).await.unwrap_err();
            assert!(format!("{err}").contains("text"));
        });
    }

    #[test]
    fn send_image_routes_to_send_media_when_capability_true() {
        let tr = Arc::new(CapturingTransport::new("telegram", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendImageTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool
                .call(json!({
                    "source": {"uri": "file:///tmp/p.png", "name": "p.png"},
                    "caption": "trend"
                }))
                .await
                .expect("call");
            assert_eq!(v["isError"], false);
        });
        let captured = tr.media.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0.kind, "image");
        assert_eq!(captured[0].0.url, "file:///tmp/p.png");
        assert_eq!(captured[0].0.filename.as_deref(), Some("p.png"));
        assert_eq!(captured[0].1.caption.as_deref(), Some("trend"));
        assert_eq!(captured[0].1.context_token, "chat-1");
    }

    #[test]
    fn send_image_returns_loud_failure_when_capability_false() {
        let tr = Arc::new(CapturingTransport::new("telegram", false));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendImageTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool
                .call(json!({"source": {"uri": "file:///tmp/p.png"}}))
                .await
                .expect("call");
            assert_eq!(v["isError"], true);
            assert!(v["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("does not support media upload"));
        });
        assert!(tr.media.lock().unwrap().is_empty());
    }

    #[test]
    fn send_image_missing_source_returns_loud_failure() {
        let tr = Arc::new(CapturingTransport::new("telegram", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendImageTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let err = tool.call(json!({})).await.unwrap_err();
            assert!(format!("{err}").contains("source"));
        });
    }

    #[test]
    fn send_file_routes_to_send_media_with_kind_file() {
        let tr = Arc::new(CapturingTransport::new("feishu", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendFileTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool
                .call(json!({"source": {"uri": "file:///tmp/r.pdf"}}))
                .await
                .expect("call");
            assert_eq!(v["isError"], false);
        });
        assert_eq!(tr.media.lock().unwrap()[0].0.kind, "file");
    }

    #[test]
    fn send_voice_routes_to_send_media_with_kind_audio() {
        let tr = Arc::new(CapturingTransport::new("wecom", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat-1".into(),
            "user-1".into(),
        ));
        let tool = SendVoiceTool { delivery };
        let rt = runtime();
        rt.block_on(async {
            let v = tool
                .call(json!({"source": {"uri": "file:///tmp/v.ogg"}}))
                .await
                .expect("call");
            assert_eq!(v["isError"], false);
        });
        assert_eq!(tr.media.lock().unwrap()[0].0.kind, "audio");
    }

    #[test]
    fn tool_registry_list_sorts_by_name() {
        let tr = Arc::new(CapturingTransport::new("telegram", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat".into(),
            "user".into(),
        ));
        let mut r = ToolRegistry::default();
        r.register(Arc::new(SendVoiceTool {
            delivery: delivery.clone(),
        }));
        r.register(Arc::new(SendTextTool {
            delivery: delivery.clone(),
        }));
        r.register(Arc::new(SendImageTool {
            delivery: delivery.clone(),
        }));
        r.register(Arc::new(SendFileTool { delivery }));
        let names: Vec<String> = r.list().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec!["send_file", "send_image", "send_text", "send_voice"]
        );
    }

    #[test]
    fn tool_registry_get_returns_handler() {
        let tr = Arc::new(CapturingTransport::new("telegram", true));
        let delivery = Arc::new(OutboundDelivery::new(
            tr.clone(),
            "chat".into(),
            "user".into(),
        ));
        let mut r = ToolRegistry::default();
        r.register(Arc::new(SendTextTool { delivery }));
        assert!(r.get("send_text").is_some());
        assert!(r.get("nonexistent").is_none());
    }
}
