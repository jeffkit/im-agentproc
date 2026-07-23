# Transport extension

The bridge's core dispatcher speaks only **generic IM DTOs**. Each concrete IM protocol (iLink today; Feishu / Telegram / … later) is an adapter that implements the `Transport` trait and translates between its own wire types and these generic DTOs. This is the seam that lets the bridge support multiple IMs without the dispatcher depending on any IM's wire protocol.

## The seam

```
                 ┌──────────────────────────────────────────┐
   IM wire ──▶   │  Transport adapter (iLink / Feishu / …)  │ ──▶ generic InboundMessage
   ◀── IM wire  │                                          │ ◀── generic OutboundReply
                 └──────────────────────────────────────────┘
                                      │
                                      ▼
                 ┌──────────────────────────────────────────┐
                 │  Dispatcher (profile run, session, loop)  │
                 └──────────────────────────────────────────┘
```

The dispatcher never sees an IM-protocol-specific type. `session_id` / `session_name` / `a2a_call_id` are **bridge-runtime** fields, first-class on the DTOs because the dispatcher needs them for routing and CLI session continuity — they are populated by the adapter (from iLink `HubExt` for the iLink-via-Hub adapter; from each IM's own conversation identifiers for others).

## The `Transport` trait

Object-safe, `Send` + `Sync`, returning boxed futures so the dispatcher can hold a `dyn Transport`:

```rust
pub trait Transport: Send + Sync {
    /// Pull the next batch of inbound messages. Updates `buf` in place
    /// when the transport uses a cursor.
    fn next_inbound<'a>(
        &'a self,
        buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>>;

    /// Send one reply.
    fn send_reply<'a>(
        &'a self,
        reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>>;

    /// Declare this transport's optional capabilities.
    fn capabilities(&self) -> TransportCapabilities;
}
```

| Type | Role |
|------|------|
| `InboundOutcome` | `Messages(Vec<InboundMessage>)` or `TokenRejected` (401/revoked → re-register). |
| `InboundMessage` | `context_token`, `from_user`, `is_from_bot`, `text`, `media: Vec<MediaRef>`, `session_id`, `session_name`, `a2a_call_id`, `extra` (IM-private), `raw` (full original JSON for diagnostics). |
| `OutboundReply` | `context_token`, `text`, `to_user`, `cli_session_id`, `session_name`, `a2a_call_id`, `usage`. |
| `SendOutcome` | `Sent` or `Throttled { ret, errmsg }` (retry with backoff). |
| `TransportCapabilities` | `media_upload: bool` (typing / read receipts are deferred — Q5). |

## Implemented adapters

| `transport:` | Status | Adapter |
|--------------|--------|---------|
| `ilink` | ✅ implemented | `IlinkTransport` — connects to iLink (via Hub or direct), long-polls inbound, sends replies. |
| any other | 🟡 placeholder | `NullTransport` — constructs successfully (proving the seam loads any adapter) but every poll returns "not implemented". Needs `--allow-null-transport` or the bridge fails fast at startup so a misconfigured transport does not back off forever as a zombie. |

## Adding a new IM (Feishu / Telegram / …)

1. **Implement `Transport`** for your IM in a new submodule under `src/bridge/transport/`. Translate your IM's inbound webhook/poll events into `InboundMessage` and your outbound sends from `OutboundReply`.
2. **Populate the bridge-runtime fields** (`session_id`, `session_name`, `a2a_call_id`) from your IM's conversation identifiers so the dispatcher can route and resume CLI sessions.
3. **Wire the factory** in `build_transport` (`src/bin/im-agentproc.rs`) to construct your adapter when `transport:` matches your IM name.
4. **Declare capabilities** — set `media_upload: true` only when your IM can upload media for outbound replies.
5. **Carry IM-private data** in `InboundMessage.extra` rather than bloating the main DTO; keep `raw` as the full original message for diagnostics.

The dispatcher, profile runner, session handling, anti-loop, and error paths are all IM-agnostic — you should not need to touch them.

## Why `NullTransport` fails fast

A placeholder that always returns "not implemented" would otherwise make the dispatcher back off forever, looking like a zombie process. The bridge therefore refuses to start a non-`ilink` transport unless you pass `--allow-null-transport` (or set `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT=1`), making the pluggability smoke test explicit rather than an accidental misconfiguration.
