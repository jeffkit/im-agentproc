//! Outbound delivery via Model Context Protocol (MCP).
//!
//! This module hosts a small MCP server (JSON-RPC 2.0 over stdio) that exposes
//! four outbound tools — `send_text`, `send_image`, `send_file`, `send_voice`
//! — for agent processes spawned by hub profiles. The server is protocol-faithful
//! to MCP 2025-06-18 ("Tools" capability) but deliberately small: no resources,
//! prompts, sampling, subscriptions, or notifications.
//!
//! ## Why a custom (non-`rmcp`) implementation
//!
//! `rmcp` 3.x is still pre-release; we want a stable, auditable, dependency-
//! light implementation that we can pin to agentproc's release cadence. The
//! MCP wire protocol on stdio is small (newline-delimited JSON-RPC 2.0 +
//! `initialize` / `tools/list` / `tools/call`) and easy to keep correct
//! against the upstream spec by hand.
//!
//! ## Success / failure semantics
//!
//! Outbound delivery follows "silent success, loud failure":
//!
//! - On success the tool returns `{content: [], isError: false}` (or an
//!   empty `content: [{"type": "text", "text": ""}]` placeholder when the
//!   schema requires at least one content block). The agent does not need
//!   to read text from a successful send — only failure matters.
//! - On failure the tool returns `{content: [{"type": "text", "text":
//!   "<reason>"}], isError: true}` so the agent can decide whether to retry.
//!
//! ## Wire format
//!
//! Stdio JSON-RPC 2.0. One JSON object per line. Notifications and requests
//! both flow over the same stream. We do **not** emit `notifications/initialized`
//! from the server; clients may not need it.

pub mod server;
pub mod tools;

pub use server::{run_server, ServerConfig};
pub use tools::{
    OutboundDelivery, SendFileTool, SendImageTool, SendTextTool, SendVoiceTool, ToolHandler,
    ToolRegistry,
};
