# IM-AgentProc

> IM-side runtime for the [agentproc](https://agentproc.dev) ecosystem — bridge an IM transport (iLink/WeChat today; more IMs via the `Transport` trait later) to local coding CLIs (Claude Code, Codex, Cursor, …) via agentproc profiles.
>
> **One inbound IM message → one agentproc profile run.**

[![crates.io](https://img.shields.io/crates/v/im-agentproc.svg)](https://crates.io/crates/im-agentproc)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

IM-AgentProc was extracted from [`ilink-hub`](https://github.com/jeffkit/ilink-hub)'s `src/bridge/` subtree. It is the agentproc-native successor of the bridge that lived inside the hub: same runtime behavior, now a standalone crate so it can grow multi-IM support without bloating the hub.

## How it fits

```
WeChat user → iLink → iLink Hub → IM-AgentProc (vtoken backend) → agentproc profile (claude-code / codex / …) → reply
```

In the default `via: hub` mode, IM-AgentProc is a *backend* of iLink Hub: it registers via `POST /hub/register`, gets a virtual token, and long-polls the Hub for inbound messages. Each message drives exactly one agentproc profile run, with CLI session continuity across turns.

## Install

```bash
cargo install im-agentproc
# or
brew tap jeffkit/tap && brew install im-agentproc
```

## Quick start

```bash
# 1. Write a bridge profile (one file == one agentproc profile)
cat > ~/.ilink-hub/ilink-hub-bridge.yaml <<'yaml'
description: claude-code on my project
agentproc:
  executor: claude-code
  cwd: ~/projects/my-app
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
yaml

# 2. Run the bridge against your iLink Hub
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export ANTHROPIC_API_KEY=sk-ant-...
im-agentproc
```

The bridge auto-registers with the Hub (no QR scan by default), gets a virtual token, and starts long-polling. Send a WeChat message to the account the Hub proxies — the reply comes back through the Hub.

## Run modes

| Command | What it does |
|---------|--------------|
| `im-agentproc` | Default: one bridge YAML, long-poll the Hub, run the profile per message. |
| `im-agentproc profile <type>` | Run a built-in profile handler as a subprocess (P0 exec protocol). Built-ins: `claude-code`, `codebuddy-code`, `codex`, `cursor`, `agy`, `recursive`. |
| `im-agentproc manager` | Supervise one child bridge per YAML in a profiles directory; each registers as an independent Hub backend. |

## Highlights

- **Pure agentproc P0 profiles** — `stdin` turn object, `stdout` NDJSON events. No bridge-specific `type:` shortcuts.
- **In-process executors** — set `executor: claude-code` (etc.) and the agentproc SDK drives the CLI directly, no bridge-subprocess fork.
- **Pluggable `Transport` trait** — the dispatcher speaks only generic IM DTOs; iLink is the only real adapter today, Feishu / Telegram / … land as new `Transport` implementations.
- **Safe by construction** — shell-`-c` + `{{MESSAGE}}` injection is rejected at load time; the message always travels via the stdin turn object.
- **Session continuity** — CLI `session_id` is persisted on the Hub and resumed on the next turn (`via: hub`).

## Documentation

Full docs (bilingual EN / 中文) live in [`docs/`](./docs) as a VitePress site:

- [What is IM-AgentProc?](./docs/guide/what-is-im-agentproc.md)
- [Quick Start](./docs/guide/quickstart.md)
- [Configuration](./docs/guide/configuration.md)
- [CLI reference](./docs/cli.md)
- [Bridge run modes](./docs/bridge/index.md)
- [Built-in profile spec](./docs/bridge/profile-spec.md)
- [Transport extension](./docs/transport.md)

Run the docs locally:

```bash
cd docs && npm install && npm run dev
```

## Relationship to the infra4agent monorepo

This crate is one sub-repo of the [infra4agent](https://github.com/jeffkit/infra4agent) logical monorepo (managed by monarbor). Cross-repo architecture and dependency edges are documented in the monorepo's `docs/ARCHITECTURE.md`. In-repo navigation lives in [`AGENTS.md`](./AGENTS.md).

## License

MIT
