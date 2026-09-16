# Transport extension

The bridge's core dispatcher speaks only **generic IM DTOs**. Each concrete IM protocol (iLink, Telegram, WeCom, Feishu, Discord; extensible to others) is an adapter that implements the `Transport` trait and translates between its own wire types and these generic DTOs. This is the seam that lets the bridge support multiple IMs without the dispatcher depending on any IM's wire protocol.

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

| `transport:` | Status | Adapter | Connection style |
|--------------|--------|---------|-----------------|
| `ilink` | ✅ implemented | `IlinkTransport` — connects to iLink via Hub (virtual-token) or direct, long-polls inbound, sends replies. | HTTP long-poll |
| `telegram` | ✅ implemented | `TelegramTransport` — Telegram Bot API `getUpdates` long-poll, sends via `sendMessage`. Credential: `im_credentials.token` / `TELEGRAM_BOT_TOKEN`. | HTTP long-poll |
| `wecom` | ✅ implemented | `WecomTransport` — WeCom Intelligent Robot WebSocket long-connection (`aibot_subscribe` / `aibot_msg_callback` / `aibot_respond_msg`). Credentials: `im_credentials.{bot_id,bot_secret}` / `WECOM_BOT_{ID,SECRET}`. | WebSocket |
| `feishu` | ✅ implemented | `FeishuTransport` — Feishu WebSocket long-connection event stream (via `larksuite-oapi-sdk-rs`), sends via IM HTTP API. Credentials: `im_credentials.{app_id,app_secret}` / `FEISHU_APP_{ID,SECRET}`. | WebSocket (SDK) |
| `discord` | ✅ implemented | `DiscordTransport` — Discord Gateway WebSocket (IDENTIFY / heartbeat / resume), sends via REST `POST /channels/{id}/messages`. Credential: `im_credentials.token` / `DISCORD_BOT_TOKEN`. | WebSocket |
| any other | 🟡 placeholder | `NullTransport` — constructs successfully (proving the seam loads any adapter) but every poll returns "not implemented". Needs `--allow-null-transport` or the bridge fails fast at startup so a misconfigured transport does not back off forever as a zombie. | — |

### Example profile YAML snippets

```yaml
# Telegram
transport: telegram
im_credentials:
  token: ${TELEGRAM_BOT_TOKEN}

# WeCom Intelligent Robot
transport: wecom
im_credentials:
  bot_id: ${WECOM_BOT_ID}
  bot_secret: ${WECOM_BOT_SECRET}

# Feishu (self-built app, WebSocket events)
transport: feishu
im_credentials:
  app_id: ${FEISHU_APP_ID}
  app_secret: ${FEISHU_APP_SECRET}

# Discord bot
transport: discord
im_credentials:
  token: ${DISCORD_BOT_TOKEN}
```

All `im_credentials` values are expanded against process environment at config load time; `${VAR}` is substituted.

## Outbound media upload

The `Transport` trait carries an optional outbound media path in addition to text replies:

```rust
async fn send_media(&self, ctx: MediaOut, media: MediaRef) -> anyhow::Result<SendOutcome>;
```

`MediaOut` is the routing half (context token + recipient + optional caption / reply-to); `MediaRef` is the payload half (`kind` + `url` + optional `filename` / `mime_type` / `size`). The default implementation refuses with `transport '<name>' does not support media upload` so adapters that haven't opted in keep working without change. Each adapter that **can** upload overrides `send_media` and reports `capabilities().media_upload == true`.

| Adapter | Outbound media path |
|---|---|
| `iLink` | not yet (deferred — uses iLink's existing text-only `aibot_respond_msg` flow) |
| `Telegram` | `sendPhoto` (kind=`image`) / `sendDocument` (everything else), multipart upload to Bot API |
| `Feishu` | Two-step: upload via `/im/v1/images` (kind=`image`) or `/im/v1/files` (kind=`file`/`video`) for `file_key`, then send via `/im/v1/messages` with the matching `msgtype` |
| `WeCom` | Inline base64 in `aibot_respond_msg` body with `msgtype` ∈ `{image, voice, file}` (WS protocol — no separate HTTP upload) |
| `Discord` | `POST /channels/{id}/messages` with `multipart/form-data` (`payload_json` + `files[0]`); `kind=image` uses the attachment array, everything else uses `sendDocument`-style upload |

`MediaRef.url` accepts `file://` / `http(s)://` / `data:` (base64). The transport helper `bridge::transport::media::read_media_bytes` resolves any of those into raw bytes before the upload call. `data:` must be base64; non-base64 data URLs are rejected.

## Outbound delivery via MCP

Text + media both reach the user through the bridge's [MCP](https://modelcontextprotocol.io/) server (`src/mcp/`). The bridge spawns an MCP server over stdio and tells the hub profile child how to connect via the standard `mcp_servers` profile block. The server exposes four tools:

| Tool | Purpose |
|---|---|
| `send_text` | Send a text reply (renders per-IM markdown / code-block conventions) |
| `send_image` | Send an image attachment |
| `send_file` | Send a generic file (PDF / docx / zip / …) |
| `send_voice` | Send an audio / voice clip |

Tool schemas follow [MCP 2025-06-18 §Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools). `source.uri` accepts the same `file://` / `http(s)://` / `data:` schemes as `MediaRef`. See [`docs/guide/mcp-outbound.md`](./guide/mcp-outbound.md) for an end-to-end quickstart.

**Success / failure envelope** (silent success, loud failure):

- Success: `{ "content": [], "isError": false }` — agents don't read text on a successful send and continue.
- Failure: `{ "content": [{"type":"text","text":"<reason>"}], "isError": true }` — agents see the reason and can retry or surface it to the user.
- Capability missing: `send_image` / `send_file` / `send_voice` return loud failure naming the transport when `capabilities().media_upload == false`. The agent should not retry — the call is structurally impossible.

## The transport registry

Transport selection is **pluggable**: the binary no longer hardcodes a `match` over transport names. `TransportRegistry` (`src/bridge/transport/registry.rs`) maps the profile's `transport:` kind string to an async factory:

```rust
pub type TransportFactory = Arc<
    dyn for<'a> Fn(&'a TransportBuildCtx)
        -> BoxFuture<'a, anyhow::Result<Arc<dyn Transport>>>
    + Send + Sync,
>;
```

A `TransportBuildCtx` carries everything an adapter may need (kind, `via`, hub/direct URLs, the `${VAR}`-expanded `im_credentials` map, explicit token, cred-file path, pairing flags, config path, interactivity) so factories never depend on CLI types. The built-in registry (`TransportRegistry::with_builtins()`) registers all in-tree adapters from the table above plus the `null` placeholder. Each adapter owns its credential parsing via `from_credentials` (profile `im_credentials:` → fallback env var) — the registry has no vendor-specific logic.

### Registering an out-of-tree adapter

Downstream crates build their own registry and register additional kinds:

```rust
let mut reg = TransportRegistry::with_builtins();
reg.register("rocketchat", |ctx| {
    Box::pin(async move {
        let t = RocketChatTransport::from_credentials(&ctx.im_credentials)?;
        Ok(Arc::new(t) as Arc<dyn Transport>)
    })
});
let t = reg.build(&ctx).await?;
```

A new IM channel therefore ships as an independent unit: implement `Transport`, `register(kind, factory)` — no edits to `src/bin/im-agentproc.rs` or `TransportKind` (which stays an open string wrapper, not a closed enum).

## Push / webhook inbound pattern

`next_inbound` is a pull API, but push-driven IMs (webhook / event callbacks) are still first-class. The blessed adapter pattern keeps the dispatcher unchanged:

1. **Adapter-hosted receiver**: the factory spawns a small HTTP/event receiver inside the adapter (like the WeCom / Feishu / Discord WebSocket workers already do) and returns immediately; the receiver authenticates and parses vendor events into `InboundMessage`.
2. **Internal buffer**: parsed messages flow into an adapter-owned `tokio::sync::mpsc` channel. Overflow policy is the adapter's choice (drop + log is fine; replies still carry `context_token`).
3. **`next_inbound` drains the buffer**: ignore the `buf` cursor (documented as allowed by the trait), await the channel, and map disconnects to `Err` / auth failures to `InboundOutcome::TokenRejected` as usual.

```
IM push ──▶ adapter receiver/worker ──▶ internal buffer ──▶ next_inbound ──▶ dispatcher
```

## Adding a new IM

1. **Implement `Transport`** for your IM in a new submodule under `src/bridge/transport/`. Translate your IM's inbound webhook/poll events into `InboundMessage` and your outbound sends from `OutboundReply`.
2. **Populate the bridge-runtime fields** (`session_id`, `session_name`, `a2a_call_id`) from your IM's conversation identifiers so the dispatcher can route and resume CLI sessions.
3. **Register the factory** under your `transport:` kind — built-in: one `register` call in `TransportRegistry::with_builtins`; external: your own registry (see above). Keep credential/env parsing inside the adapter (`from_credentials`).
4. **Override `send_media`** if your IM can upload attachments — use `bridge::transport::media::read_media_bytes` to fetch the bytes from `MediaRef.url` (file / http(s) / base64 data URLs). Set `capabilities().media_upload = true` so the MCP server routes `send_image` / `send_file` / `send_voice` calls to you.
5. **Carry IM-private data** in `InboundMessage.extra` rather than bloating the main DTO; keep `raw` as the full original message for diagnostics.

The dispatcher, profile runner, session handling, anti-loop, and error paths are all IM-agnostic — you should not need to touch them.

## Why `NullTransport` fails fast

A placeholder that always returns "not implemented" would otherwise make the dispatcher back off forever, looking like a zombie process. The bridge therefore refuses to start a transport whose kind has no registered factory unless you pass `--allow-null-transport` (or set `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT=1`), making the pluggability smoke test explicit rather than an accidental misconfiguration.

## Testing the factory

The factory dispatch lives in `TransportRegistry` (`src/bridge/transport/registry.rs`); the binary's `build_transport` only assembles a `TransportBuildCtx` and delegates. Unit tests exercise it without needing real IM credentials; they cover:

- `build_transport_direct_bails_without_base_url` — `via: direct` without `base_url:` fails fast at the M2 gate.
- `build_transport_unknown_transport_bails_without_allow_flag` — L4 guard: unknown `transport:` refuses to start a zombie.
- `build_transport_unknown_transport_succeeds_with_allow_flag` — `--allow-null-transport` actually loads the placeholder.
- `build_transport_telegram_bails_without_token` / `_wecom_bails_without_credentials` / `_wecom_bails_without_bot_secret` / `_feishu_bails_without_app_secret` / `_discord_bails_without_token` — each real adapter errors out fast with a clear message naming the missing key when neither `im_credentials:` nor the fallback env var is set.
- `build_transport_telegram_falls_back_to_env_token` — proves the `im_credentials.*` → `*_BOT_TOKEN` env fallback chain works.

The credential-fallback tests serialise on `ENV_LOCK` and call `clear_im_credential_env()` to keep them hermetic regardless of what is set in the developer's shell.

**Not covered by unit tests today (would need network mocks):** the full happy-path inbound + outbound cycle of each adapter. The constructor itself for `TelegramTransport::new(token)` is purely local (only builds a `reqwest::Client`) so the env-fallback test doubles as a happy-path smoke; `FeishuTransport::new` / `WecomTransport::new` / `DiscordTransport::new` each `tokio::spawn` a background WebSocket worker in their constructor and so need a fake gateway / mock server to assert against. Add those as e2e tests under `tests/` when network mocking infra lands.
