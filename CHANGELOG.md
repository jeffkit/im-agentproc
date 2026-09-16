# Changelog

All notable changes to `im-agentproc` are documented here. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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