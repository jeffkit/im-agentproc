# What is IM-AgentProc?

IM-AgentProc is the **IM-side runtime** of the [agentproc](https://github.com/jeffkit/agentproc) ecosystem. It bridges an IM transport to local coding CLIs (Claude Code, Codex, Cursor, …) via agentproc profiles: **one inbound IM text message → one agentproc profile run**.

## Why it exists

IM-AgentProc was extracted from [`ilink-hub`](https://github.com/jeffkit/ilink-hub)'s `src/bridge/` subtree. The bridge that turned WeChat messages into local CLI runs used to live inside the hub. As it grew clear that the same bridge logic could serve more than one IM (Feishu, Telegram, …), the bridge was split out into its own crate so it could grow multi-IM support without bloating the hub.

The split rationale and remaining in-crate decoupling work are tracked in the proposal `bridge-as-multi-im-runtime` (Appendix A).

## What it is not

- **Not a messaging server.** It does not hold WeChat connections itself in `via: hub` mode — it registers as a *backend* of iLink Hub and long-polls the Hub for inbound messages. (`via: direct` connects straight to the real iLink upstream, but that is for single-bridge debugging, not a deployment topology.)
- **Not an orchestrator.** It runs one profile per message. Multi-step flows, HITL, and quality gates live in [flowcast](https://github.com/jeffkit/flowcast) / [plaita](https://github.com/jeffkit/plaita), which can themselves drive agentproc.
- **Not a new protocol.** Profiles are pure [agentproc P0](https://agentproc.dev/spec/) — `stdin` turn object, `stdout` NDJSON events. Anything that speaks agentproc works as a profile handler.

## Where it sits in the stack

```
WeChat user
   │  (WeChat)
   ▼
iLink official API
   │
   ▼
iLink Hub ── multiplexes one WeChat account to many backends
   │  (virtual token, long-poll)
   ▼
IM-AgentProc ── one bridge process per profile YAML
   │  (agentproc P0: stdin turn → stdout NDJSON)
   ▼
agentproc profile ── claude-code / codex / cursor / your own script
```

In `via: hub` mode (default), IM-AgentProc is a *consumer* of iLink Hub, not a sibling of it. The Hub owns the single WeChat connection; IM-Agentproc is one of possibly many backends the Hub fans messages out to.

## Two connection modes

| `via:` | Connects to | Credentials | Use when |
|--------|--------------|-------------|----------|
| `hub` (default) | iLink Hub (`/hub/register` → virtual token, or `--pair` QR, or explicit `WEIXIN_TOKEN`) | Auto-registered vtoken, saved at `~/.ilink-hub/bridge-credentials.json` | Normal operation — you already run iLink Hub |
| `direct` | The real iLink upstream (e.g. `https://ilinkai.weixin.qq.com`) | Explicit `WEIXIN_TOKEN` (pre-provisioned bot_token) or QR login against the upstream | Single-bridge debugging without a Hub. Requires `base_url:` (or a non-default `WEIXIN_BASE_URL`); refuses the localhost Hub default to avoid silently targeting a Hub |

`via: direct` **cannot resume CLI sessions across messages** — the real upstream does not echo the `session_id` the Hub persists in `via: hub` mode. Each message starts a fresh CLI session.

## Three run modes

| Command | What it does |
|---------|--------------|
| `im-agentproc` (default) | Load one bridge YAML, connect to the Hub, long-poll for messages, run the profile per message. |
| `im-agentproc profile <type>` | Run a **built-in** profile handler as a subprocess (P0 exec protocol: read turn from stdin, write NDJSON to stdout). No Hub connection. Used by profiles that spawn `im-agentproc profile <type>` as their `command`. |
| `im-agentproc manager` | Scan a profiles directory; supervise one child bridge per `*.yaml` file, each registering as an independent Hub backend. |

See the [CLI reference](/cli) for every flag.

## Relationship to agentproc and the hub

- **agentproc** is the shared protocol + SDK. IM-AgentProc depends on the `agentproc` Rust crate (crates.io `0.11+`) for the turn object, NDJSON events, and the `run()` runner.
- **iLink Hub** owns the WeChat connection and multiplexes it. IM-AgentProc is one of its backends.
- **Built-in profiles** (`im-agentproc profile <type>`) are themselves agentproc 0.4 *agents*: they read the turn from stdin and emit NDJSON on stdout, exactly like any external script or SDK handler.

Next: [Quick Start](/guide/quickstart) →
