//! Minimal MCP stdio server.
//!
//! Reads newline-delimited JSON-RPC 2.0 requests from stdin, dispatches them
//! to the registered tools, and writes newline-delimited responses to
//! stdout. Only the three methods required by MCP 2025-06-18 §Tools are
//! implemented:
//!
//! - `initialize` → returns protocol version + server info + capabilities.
//! - `tools/list` → returns the registered tool descriptors (name +
//!   description + JSON Schema for `arguments`).
//! - `tools/call` → invokes a tool handler with the supplied arguments.
//!
//! Unknown methods get a JSON-RPC `-32601 Method not found` error so the
//! client can surface a clear diagnostic instead of silently ignoring them.

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::tools::ToolRegistry;

/// JSON-RPC protocol version string we advertise. MCP 2025-06-18 sets
/// `2025-06-18`; the actual wire shape matches the current spec's `Tools`
/// section. Older clients may send `2024-11-05` — we accept any string and
/// echo our version back per the spec's "no negotiation" rule.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Server identity surfaced via `serverInfo`.
const SERVER_NAME: &str = "im-agentproc-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Configuration for [`run_server`]. Kept tiny so callers can construct it
/// from `Arc<dyn Transport>` plus a hub profile's working directory.
pub struct ServerConfig {
    /// Registered tools. `Arc` so cloning the config doesn't move ownership
    /// of the registry in unexpected ways.
    pub registry: Arc<ToolRegistry>,
}

impl ServerConfig {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

/// Top-level entry point. Blocks until EOF on stdin; returns on EOF, on a
/// malformed JSON line (warn + skip), or on a protocol-level error
/// reported via JSON-RPC `error`.
pub async fn run_server(cfg: ServerConfig) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut reader = stdin.lock();

    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            // EOF — the hub profile child process closed our stdin. Clean exit.
            debug!("MCP server reached stdin EOF");
            return Ok(());
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, line, "MCP: malformed JSON-RPC frame");
                // Per JSON-RPC 2.0: parse errors get -32700 and we MUST NOT
                // include an `id` (which we couldn't have parsed).
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": "Parse error"},
                });
                writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                out.flush()?;
                continue;
            }
        };

        // Notifications (no `id`) are accepted and discarded. The MCP spec
        // currently has no server-bound notifications we care about
        // (`notifications/initialized` is client→server and we don't act
        // on it).
        let id = request.get("id").cloned();
        if id.is_none() {
            debug!(
                method = request.get("method").and_then(serde_json::Value::as_str),
                "MCP: notification (no id), discarding"
            );
            continue;
        }

        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let response = handle(method, params, &cfg).await;
        let framed = json!({
            "jsonrpc": "2.0",
            "id": id.unwrap(),
            "result": response,
        });
        writeln!(out, "{}", serde_json::to_string(&framed)?)?;
        out.flush()?;
    }
}

async fn handle(method: &str, params: Value, cfg: &ServerConfig) -> Value {
    match method {
        "initialize" => initialize_result(params),
        "tools/list" => tools_list_result(cfg),
        "tools/call" => tools_call_result(params, cfg).await,
        "ping" => json!({}),
        other => {
            warn!(method = other, "MCP: unknown method");
            json!({
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {other}"),
                }
            })
        }
    }
}

fn initialize_result(params: Value) -> Value {
    // Per MCP spec, the server returns its protocol version + info + capabilities.
    // We don't negotiate: if the client sent a `protocolVersion` we acknowledge
    // it back as-is (presence-as-feature). We declare only `tools` capability.
    let _client_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
        "capabilities": {
            "tools": {"listChanged": false},
        },
    })
}

fn tools_list_result(cfg: &ServerConfig) -> Value {
    let tools: Vec<Value> = cfg
        .registry
        .list()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

#[derive(Debug, Deserialize)]
struct ToolsCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

async fn tools_call_result(params: Value, cfg: &ServerConfig) -> Value {
    let parsed: ToolsCallParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(err) => {
            return json!({
                "isError": true,
                "content": [{"type": "text", "text": format!("invalid tools/call params: {err}")}],
            });
        }
    };
    let Some(handler) = cfg.registry.get(&parsed.name) else {
        return json!({
            "isError": true,
            "content": [{"type": "text", "text": format!("unknown tool: {}", parsed.name)}],
        });
    };
    match handler.call(parsed.arguments).await {
        Ok(call_result) => call_result,
        Err(err) => {
            // "loud failure" — surface the error message in a text content
            // block so the agent can read it.
            json!({
                "isError": true,
                "content": [{"type": "text", "text": format!("{err:#}")}],
            })
        }
    }
}

/// Helper used by tests to render a success call result. Lives here so the
/// tools themselves can stay focused on payload assembly.
pub(crate) fn success_empty() -> Value {
    // Silent success: no content. Agents that look for `isError: false`
    // short-circuit and continue.
    json!({ "content": [], "isError": false })
}

/// 带 text 的 silent success（与 failure_text 配对）。当前生产成功路径用
/// 空 content（`content:[]`），此函数为测试与未来"成功带说明"场景预留。
#[allow(dead_code)]
pub(crate) fn success_text(text: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": false,
    })
}

pub(crate) fn failure_text(text: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::transport::{
        MediaOut, MediaRef, SendOutcome, Transport, TransportCapabilities,
    };
    use crate::mcp::tools::{ToolDescriptor, ToolHandler};
    use async_trait::async_trait;
    use futures_util::future::BoxFuture;
    use std::sync::Mutex;

    /// A fake `Transport` whose `send_reply` / `send_media` records what was
    /// passed so tests can assert on the call.
    /// 测试辅助 transport（send_reply/send_media 记录入参供断言）。
    #[allow(dead_code)]
    struct CapturingTransport {
        last_text: Arc<Mutex<Option<String>>>,
        last_media: Arc<Mutex<Option<MediaRef>>>,
    }

    #[allow(dead_code)]
    impl CapturingTransport {
        fn new() -> Self {
            Self {
                last_text: Arc::new(Mutex::new(None)),
                last_media: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl Transport for CapturingTransport {
        fn next_inbound<'a>(
            &'a self,
            _buf: &'a mut String,
        ) -> BoxFuture<'a, anyhow::Result<crate::bridge::transport::InboundOutcome>> {
            Box::pin(async move { Ok(crate::bridge::transport::InboundOutcome::Messages(vec![])) })
        }
        fn send_reply<'a>(
            &'a self,
            reply: crate::bridge::transport::OutboundReply,
        ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
            let last_text = self.last_text.clone();
            Box::pin(async move {
                *last_text.lock().unwrap() = Some(reply.text);
                Ok(SendOutcome::Sent)
            })
        }
        fn send_media<'a>(
            &'a self,
            _ctx: MediaOut,
            media: MediaRef,
        ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
            let last_media = self.last_media.clone();
            Box::pin(async move {
                *last_media.lock().unwrap() = Some(media);
                Ok(SendOutcome::Sent)
            })
        }
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> TransportCapabilities {
            TransportCapabilities { media_upload: true }
        }
    }

    struct EchoTool;

    #[async_trait]
    impl ToolHandler for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "echo".into(),
                title: "Echo".into(),
                description: "Echoes the input text".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                }),
            }
        }
        async fn call(&self, args: Value) -> Result<Value> {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(success_text(format!("echo: {text}")))
        }
    }

    struct FailTool;

    #[async_trait]
    impl ToolHandler for FailTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "fail".into(),
                title: "Fail".into(),
                description: "Always fails".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(failure_text("forced failure"))
        }
    }

    fn registry_with(tools: Vec<Arc<dyn ToolHandler>>) -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::default();
        for t in tools {
            r.register(t);
        }
        Arc::new(r)
    }

    #[test]
    fn initialize_returns_protocol_version_and_tools_capability() {
        let cfg = ServerConfig::new(registry_with(vec![]));
        let resp = tokio_test_block_on(handle(
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
            &cfg,
        ));
        assert_eq!(resp["protocolVersion"], "2025-06-18");
        assert_eq!(resp["serverInfo"]["name"], "im-agentproc-mcp");
        assert_eq!(resp["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn tools_list_renders_descriptor_shape() {
        let cfg = ServerConfig::new(registry_with(vec![Arc::new(EchoTool)]));
        let resp = tokio_test_block_on(handle("tools/list", json!({}), &cfg));
        let tools = resp["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[0]["title"], "Echo");
        assert!(tools[0]["inputSchema"]["properties"]["text"].is_object());
    }

    #[test]
    fn tools_call_routes_to_handler() {
        let cfg = ServerConfig::new(registry_with(vec![Arc::new(EchoTool)]));
        let resp = tokio_test_block_on(handle(
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "hi"}}),
            &cfg,
        ));
        assert_eq!(resp["isError"], false);
        assert_eq!(resp["content"][0]["text"], "echo: hi");
    }

    #[test]
    fn tools_call_unknown_tool_returns_loud_failure() {
        let cfg = ServerConfig::new(registry_with(vec![]));
        let resp = tokio_test_block_on(handle(
            "tools/call",
            json!({"name": "nope", "arguments": {}}),
            &cfg,
        ));
        assert_eq!(resp["isError"], true);
        assert!(resp["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nope"));
    }

    #[test]
    fn tools_call_handler_failure_passes_through() {
        let cfg = ServerConfig::new(registry_with(vec![Arc::new(FailTool)]));
        let resp = tokio_test_block_on(handle(
            "tools/call",
            json!({"name": "fail", "arguments": {}}),
            &cfg,
        ));
        assert_eq!(resp["isError"], true);
        assert_eq!(resp["content"][0]["text"], "forced failure");
    }

    #[test]
    fn unknown_method_returns_jsonrpc_error() {
        let cfg = ServerConfig::new(registry_with(vec![]));
        let resp = tokio_test_block_on(handle("does/not/exist", json!({}), &cfg));
        assert_eq!(resp["error"]["code"], -32601);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does/not/exist"));
    }

    /// Minimal block_on — we don't want to drag in `tokio::main` for unit tests
    /// that only need to drive a single async call. The MCP server uses
    /// `tokio::spawn` for background tasks but the call path itself is just
    /// `await` over a future.
    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn success_empty_serializes_with_no_content_blocks() {
        let v = success_empty();
        assert_eq!(v["content"].as_array().unwrap().len(), 0);
        assert_eq!(v["isError"], false);
    }

    #[test]
    fn success_text_serializes_with_single_text_block() {
        let v = success_text("ok");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "ok");
        assert_eq!(v["isError"], false);
    }

    #[test]
    fn failure_text_serializes_with_is_error_true() {
        let v = failure_text("nope");
        assert_eq!(v["isError"], true);
        assert_eq!(v["content"][0]["text"], "nope");
    }
}
