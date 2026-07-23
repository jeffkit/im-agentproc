# Quick Start

Get from zero to a running IM → CLI bridge in a few minutes.

## Prerequisites

- An **iLink Hub** running and reachable (default `http://127.0.0.1:8765`). IM-AgentProc is a *backend* of the Hub in the default `via: hub` mode, not a replacement for it.
- The **target CLI** your profile wraps, installed and logged in (e.g. `claude`, `codex`, `cursor`, …). The bridge probes the CLI at startup and exits if it is missing.
- API keys the profile needs, available in the bridge's environment (e.g. `ANTHROPIC_API_KEY`).

## 1. Install

::: code-group

```bash [cargo]
cargo install im-agentproc
```

```bash [brew]
brew tap jeffkit/tap
brew install im-agentproc
```

```bash [from source]
git clone https://github.com/jeffkit/im-agentproc
cd im-agentproc
cargo build --release
# → target/release/im-agentproc
```

:::

Verify:

```bash
im-agentproc --version
```

## 2. Write a bridge profile

A bridge YAML is **one file == one agentproc profile** in spec-aligned hub form. Create `~/.ilink-hub/ilink-hub-bridge.yaml`:

```yaml
description: claude-code on my project
agentproc:
  executor: claude-code          # in-process executor (agentproc SDK ≥ 0.10)
  cwd: ~/projects/my-app         # where the CLI runs
  streaming: true
  timeout_secs: 1800
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

That's it. The `agentproc:` block is field-for-field the agentproc profile spec. No `type:` shortcuts, no bridge-specific extensions.

::: tip No `command` needed with an executor
When `executor:` is set and recognised by the agentproc SDK, the runner drives the CLI directly — `command` may stay empty. Without an executor, `command` (or the `script:` shorthand) is required.
:::

## 3. Run the bridge

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765   # your iLink Hub
export ANTHROPIC_API_KEY=sk-ant-...            # picked up via ${ANTHROPIC_API_KEY}
im-agentproc
```

On first run with no saved credentials, the bridge **auto-registers** with the Hub via `POST /hub/register`, writes a virtual token to `~/.ilink-hub/bridge-credentials.json`, and starts long-polling. No QR scan needed by default.

::: tip Hub admin token
If your Hub enforces admin auth (`ILINK_ADMIN_TOKEN` on the Hub side), set the same variable in the bridge's environment before starting it — auto-registration otherwise fails with HTTP 401.
:::

## 4. Send a message

Send a WeChat message to the account the Hub proxies. The bridge:

1. Receives the message from the Hub
2. Builds an agentproc turn object and runs the profile (`claude-code` here)
3. Streams `partial` chunks back through the Hub as they arrive (when `streaming: true`)
4. Sends the final reply

Reply to the same WeChat conversation to continue — the CLI session id is persisted on the Hub and resumed on the next turn.

## 5. Run multiple profiles with the manager

To run several profiles side by side, each as an independent Hub backend:

```bash
# Put one YAML per profile in a directory
mkdir -p ~/.ilink-hub-bridge/profiles
cp profile-a.yaml ~/.ilink-hub-bridge/profiles/
cp profile-b.yaml ~/.ilink-hub-bridge/profiles/

im-agentproc manager
```

The manager scans `~/.ilink-hub-bridge/profiles` every 5s, spawns one child bridge per YAML (named after the file stem), and restarts crashed children with exponential backoff. Each child gets its own credential JSON under `~/.ilink-hub-bridge/credentials/`.

## Other ways to connect

| Goal | Command |
|------|---------|
| Explicit virtual token | `im-agentproc --token <vtoken>` (or `WEIXIN_TOKEN`) |
| Hub QR pairing (phone confirm) | `im-agentproc --pair` |
| Re-register when the saved token is invalid | `im-agentproc --force-register` |
| Connect straight to the real iLink upstream | `via: direct` + `base_url:` in the YAML (see [What is IM-AgentProc?](/guide/what-is-im-agentproc#two-connection-modes)) |

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Startup probe failed for profile …` | The wrapped CLI is missing or not on `PATH`. Install/log in to the CLI, or set `command` to an absolute path. |
| `WEIXIN_TOKEN / --token 被拒绝` | The token was revoked. Re-register: `im-agentproc --force-register` (hub mode) or `--pair` (direct mode). |
| `CLI 认证失败` | The CLI's own auth expired (e.g. `claude` logged out). Re-login to the CLI and restart the bridge. |
| HTTP 401 on auto-register | The Hub enforces admin auth. Set `ILINK_ADMIN_TOKEN` (matching the Hub) in the bridge environment. |
| `via: direct 需要显式 base_url` | Direct mode refuses the localhost Hub default. Set `base_url:` in the YAML or a non-default `WEIXIN_BASE_URL`. |
| `transport … 没有真实适配器` | You set `transport:` to something other than `ilink`. Only `ilink` is implemented today; add `--allow-null-transport` for a pluggability smoke test. |

Next: [Configuration](/guide/configuration) →
