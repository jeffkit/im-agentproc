# Changelog

All notable changes to `im-agentproc` are documented here. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Channel text limit in `TransportCapabilities`.** `max_text_len: Option<usize>`
  (`None` = the adapter declares no client-visible cap). Telegram declares
  `4096`, Discord `2000`; ilink / wecom / feishu declare `None`.
- **`SendOutcome::Rejected { ret, errmsg }`** for deterministic platform
  rejections, so the dispatcher can tell them apart from retryable throttles.

### Changed

- Long replies are split by `min(profile.max_reply_chars, transport.max_text_len)`
  instead of the profile cap alone, so an 8000-char body no longer overruns the
  Telegram (4096) / Discord (2000) channel limit.
- A deterministically rejected part is no longer retried against the 60-300s
  backoff budget: it is dropped immediately, degraded once into half-sized
  sub-parts, and the remaining parts are still delivered (previously the whole
  remainder of the reply was lost behind a dead `?`).
- Telegram 4xx `sendMessage` errors, Discord client errors (400 code 50035 /
  413) and Feishu `99991400` (message too long, previously mapped to
  `Throttled`) now surface as `SendOutcome::Rejected`.

### Fixed

- **Fatal-auth classification narrowed to full phrases.** `AUTH_ERROR_KEYWORDS`
  no longer matches the bare words `token` / `auth` / `401` / `login`, and the
  dispatcher no longer treats `not found` / `no such file` as credential
  failures. A single message carrying `max_tokens`, `tokens exceeded`,
  `model not found`, `tool not found` or `404 not found` used to be fatal and
  exited the whole bridge process, interrupting every session on that profile.
- The `Fatal` decision is now made by the single predicate
  `bridge::is_fatal_auth_error()`; `probe.rs` classification reuses it.
- **Per-turn MCP conversation context.** The bridge no longer reads
  `IM_AGENTPROC_MCP_CONTEXT_TOKEN` / `IM_AGENTPROC_MCP_TO_USER` from its own
  process environment. `mcp_extra_env_for_profile()` now takes the current
  inbound message's `context_token` / `from_user` and overwrites both keys on
  every turn, so concurrent conversations can no longer cross-deliver outbound
  `send_text` / `send_image` / `send_file` / `send_voice` calls (#4).
  Credential keys (`IM_AGENTPROC_MCP_TRANSPORT`, the per-transport secrets,
  `IM_AGENTPROC_MCP_ILINK_*`) are still forwarded from the bridge process env.

### Breaking

- `TransportCapabilities` gained the `max_text_len` field and `SendOutcome`
  gained the `Rejected` variant. Downstream crates that build
  `TransportCapabilities` with a struct literal or exhaustively `match` on
  `SendOutcome` must be updated (`Default` still covers `::default()`).

## [0.3.0] - 2026-09-16

### Added

- **Pluggable transport registry.** New `bridge::transport::registry` maps the
  profile's `transport:` kind to an async factory (`TransportRegistry` +
  `TransportBuildCtx`), replacing the hardcoded `match` in the binary. Built-ins
  register `ilink` (hub/direct), `telegram`, `wecom`, `feishu`, `discord` and the
  `null` placeholder; downstream crates can `register(kind, factory)` their own
  adapters. Credential parsing (incl. env fallback) moved into each adapter's
  `from_credentials`. Unknown kinds still fail fast unless
  `--allow-null-transport` (L4); the direct-mode M2 guard is unchanged.
- **Push/webhook adapter pattern** documented in `docs/transport.md`
  (adapter-hosted receiver → internal buffer → `next_inbound` drain).
- **Library run entry for downstream channels.** The default bridge run moved
  from the binary into the library (`bridge::run_loop::run_bridge_reconnecting(BridgeRunOptions)`),
  with the transport registry supplied by the caller — a downstream crate can
  ship a thin `main` registering custom `transport:` kinds with zero edits to
  this repository. See `examples/custom_transport_main.rs`.
- **MCP subprocess via the registry.** `im-agentproc mcp-server` resolves its
  transport through `TransportRegistry::with_mcp_builtins()` (hardcoded `match`
  removed). Credential env vars follow the uniform
  `IM_AGENTPROC_MCP_{KIND}_{KEY}` scheme (`{KEY}` lowercased into
  `im_credentials`); custom kinds registered in the MCP registry resolve too.

## [0.2.0] - 2026-07-27

### Added

- **Outbound media via MCP.** New `src/mcp/` module hosts a small JSON-RPC 2.0
  stdio server (no `rmcp` dep) exposing four outbound delivery tools —
  `send_text`, `send_image`, `send_file`, `send_voice` — to hub profile child
  processes. Success returns `content:[] isError:false` (silent); failure
  returns `content:[{type:text,text:...}] isError:true` (loud). The bridge
  ships a new `im-agentproc mcp-server` sub-command plus `IM_AGENTPROC_MCP_*`
  env vars for the bridge manager to launch per-child.

- **`Transport::send_media()` + `MediaOut` context.** Default implementation
  refuses with a clear error so existing adapters stay unchanged; `Telegram`,
  `Feishu`, `WeCom`, and `Discord` override. iLink deferred (needs
  `getuploadurl` + upload URL PUT). `TransportCapabilities::media_upload`
  now reflects which adapters actually upload.

- **`Transport::name() -> &'static str`.** Each adapter reports its stable
  YAML-key name for use in error messages and capability inspection.

- **Inbound attachment normalisation.** New `attachments.rs` enforces the
  `turn.attachments[]` schema: kind ∈ `{image, file, audio, video, other}`,
  url ∈ `{file, http, https, data}`, relative `file://` paths resolved
  against `cwd`, missing files warned-and-dropped.

### Changed

- **Telegram / Feishu / Discord transports** accept an explicit API base url
  (`with_base_url` / `with_api_base` constructors) so unit tests can point
  them at a local mockito server. Production callers continue to use `new`.

- **Feishu inbound** thread `api_base` through to the resource-download path
  so the same override applies to inbound media downloads, not just outbound
  uploads.

- **agentproc spec.** `spec/protocol.md` `attachments` field tightened to
  the controlled vocabulary + allowed schemes (see "Inbound attachment
  normalisation" above). Editorial only — wire protocol unchanged.

## [0.1.1] - 2026-07-25

### Changed

- Depend on `agentproc` 0.11.x from crates.io (no longer git rev pin).
- Bump to `0.1.1` for the `im-agentproc` crates.io track.

## [0.1.0] - 2026-07-20

### Added

- Initial release: `Transport` trait seam + adapters for iLink, Telegram,
  WeCom, Feishu, Discord. Extracted from `ilink-hub`'s `src/bridge/`
  subtree.