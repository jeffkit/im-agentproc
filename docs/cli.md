# CLI reference

The `im-agentproc` binary has three run modes: **default** (one bridge, long-poll the Hub), **`profile`** (run a built-in profile handler as a subprocess), and **`manager`** (supervise many child bridges).

```bash
im-agentproc [global flags] [subcommand]
```

With no subcommand, the binary runs in default bridge mode.

## Global flags

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--hub-url <url>` | `WEIXIN_BASE_URL` | `http://127.0.0.1:8765` | Hub base URL (same key other backends use). |
| `--token <tok>` | `WEIXIN_TOKEN` | — | Explicit virtual token. Omit to use saved creds, auto-register, or `--pair`. |
| `--cred-file <path>` | `ILINKHUB_BRIDGE_CREDS` | per-`via:` default | Local credential JSON path. |
| `--pair` | — | `false` | Ignore saved creds and run QR pairing (phone confirm). |
| `--register-name <n>` | `ILINKHUB_BRIDGE_REGISTER_NAME` | `local-<host>-<stem>` | Stable client name when auto-registering via `/hub/register`. |
| `--force-register` | — | `false` | If the cred file exists but is invalid/empty, delete it and auto-register again. |
| `--allow-null-transport` | `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT` | `false` | Allow a non-`ilink` transport's placeholder adapter (pluggability smoke test). |
| `--no-interactive` | `ILINKHUB_BRIDGE_NON_INTERACTIVE` | `false` | Disable QR flows. `via: direct` bails instead of printing a QR when stdout is not a TTY. |
| `--config <path>` | — | `~/.ilink-hub/ilink-hub-bridge.yaml` | Bridge YAML path. Default mode only. |
| `--version` | — | — | Print version and exit. |
| `--help`, `-h` | — | — | Show help. |

## Default mode — `im-agentproc`

Load one bridge YAML, connect to the Hub, long-poll for inbound messages, run the profile per message.

```bash
im-agentproc --config ~/.ilink-hub/ilink-hub-bridge.yaml
```

Credential resolution (in order):

1. **Explicit token** — `--token` / `WEIXIN_TOKEN` if non-empty.
2. **QR pairing** — `--pair` runs Hub QR pairing (phone confirm).
3. **Zero-interaction (default)** — when no token and no cred file, the process calls `POST /hub/register`, writes a virtual token to the local JSON. If the cred file exists but is invalid/empty, it is **not** silently overwritten (to protect a prior QR pairing) — use `--token`, `--pair`, or `--force-register`.

If the Hub enforces admin auth (`ILINK_ADMIN_TOKEN`), set the same variable in the bridge environment before starting.

### Runtime stop reasons

| `BridgeStop` | Meaning |
|--------------|---------|
| `Shutdown` | Graceful shutdown (Ctrl-C / SIGTERM). |
| `TokenRejected` (explicit token) | The token was rejected; bail with a hint to re-register or re-pair. |
| `TokenRejected` (saved creds) | Token revoked at runtime; delete the cred file and reconnect. |
| `FatalCliError` | The CLI's own auth failed (e.g. `claude` logged out). Needs a human fix and a bridge restart. |

## `im-agentproc profile <type>`

Run a **built-in** profile handler as a subprocess. No Hub connection — it reads the agentproc turn object from stdin and writes NDJSON events to stdout, exactly like any external agentproc agent. Profiles that wrap a built-in CLI spawn `im-agentproc profile <type>` as their `command`:

```yaml
agentproc:
  command: im-agentproc
  args: ["profile", "recursive"]
  cwd: ~/projects/recursive
```

### Built-in types

| `<type>` | CLI tool | Session resume | Notes |
|----------|---------|-----------------|-------|
| `claude-code` | `claude` | ✓ (`--resume`) | Anthropic Claude Code; multimodal + permission channel supported |
| `codebuddy-code` | `codebuddy` | ✓ (`--resume`) | CodeBuddy Code (stream-json compat) |
| `codex` | `codex` | ✗ | OpenAI Codex CLI |
| `cursor` | `cursor` | ✓ (optional) | Cursor background agent CLI |
| `agy` | `agy` | ✓ (`--conversation`) | Google Antigravity CLI |
| `recursive` | `recursive` | ✓ (`-r`) | Recursive agent CLI (session UUID from stderr) |

Each handler: reads the turn from stdin → runs the CLI (resuming when a session exists, falling back to a fresh session on resume failure) → streams `partial` events → emits one terminal `result` (with optional `session_id`). See the [built-in profile spec](/bridge/profile-spec).

## `im-agentproc manager`

Discover profile YAML files in a directory and supervise one child bridge per file. Each child derives a stable workspace/register name from the file stem and stores a separate credential JSON, so every child registers as an independent Hub backend.

```bash
im-agentproc manager \
  --profiles-dir ~/.ilink-hub-bridge/profiles \
  --credentials-dir ~/.ilink-hub-bridge/credentials \
  --scan-interval-secs 5 \
  --restart-backoff-secs 5 \
  --max-restart-backoff-secs 60
```

| Flag | Default | Description |
|------|---------|-------------|
| `--profiles-dir <dir>` | `~/.ilink-hub-bridge/profiles` | Directory of bridge profile YAMLs (`*.yaml` / `*.yml`). |
| `--credentials-dir <dir>` | `~/.ilink-hub-bridge/credentials` | Per-profile credential JSON directory. |
| `--scan-interval-secs <n>` | `5` | Seconds between profile directory scans. |
| `--restart-backoff-secs <n>` | `5` | Minimum seconds before restarting an exited child. |
| `--max-restart-backoff-secs <n>` | `60` | Cap for exponential restart backoff. |

The manager ignores `--token`, `--cred-file`, `--register-name`, and `--pair` (each child auto-registers independently). It injects `ILINKHUB_BRIDGE_NON_INTERACTIVE` into children so they fail fast instead of QR-blocking headless, and propagates `ILINK_ADMIN_TOKEN` so children can auto-register when the Hub enforces admin auth.

::: warning Never share a vtoken across children
If `ILINK_ADMIN_TOKEN` is missing and the Hub enforces admin auth, children fail to auto-register (HTTP 401). Do **not** work around this by reusing another backend's credentials — sharing a vtoken makes bridges compete for the same message queue (split-brain). Set `ILINK_ADMIN_TOKEN` instead.
:::

## Debugging

```bash
# Dump every inbound WeixinMessage JSON + item_list[*].extra to stderr
ILINKHUB_BRIDGE_DUMP_MSG=1 im-agentproc

# Bump tracing
RUST_LOG=im_agentproc=debug im-agentproc
```
