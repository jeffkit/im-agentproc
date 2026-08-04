# Configuration

A bridge profile is **one YAML file == one agentproc profile**, in spec-aligned hub form: a pure agentproc execution config nested under `agentproc:`, with a few ilink-hub siblings (`description`, `script`, `transport`, `via`, `base_url`).

## Full example

```yaml
description: issue-keeper on MiniMax          # surfaced via the Hub MCP list_agents tool
script: ./my-handler.py                       # optional shorthand (expanded to command/args)
transport: ilink                              # ilink | telegram | wecom | feishu | discord
via: hub                                      # default; or `direct` to skip the Hub (ilink only)
base_url: https://ilinkai.weixin.qq.com       # only used by `via: direct`
im_credentials: {}                            # transport-specific; ${VAR} expanded at load time
agentproc:
  executor: claude-code                       # optional in-process executor
  command: python3                             # argv[0] — single token, never split
  args: ["./bridge.py"]                        # argv[1..]
  cwd: ~/projects/my-app                       # working dir; relative resolves against {{PROFILE_DIR}}
  timeout_secs: 1800                           # default 1800 (30 min)
  kill_grace_secs: 5                           # SIGTERM → SIGKILL grace
  max_reply_chars: 8000                        # default 8000
  truncation_suffix: "\n\n…(输出已截断)"
  streaming: true                              # default true
  permission: false                            # default false (agentproc 0.4 perm channel)
  include_stderr_in_reply: false
  send_error_reply: true                       # default true
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}    # ${VAR} expanded at run time
    CLAUDE_MODEL: glm-5.2
  env_allowlist:                               # restrict which ${VAR} may expand
    - ANTHROPIC_API_KEY
    - CLAUDE_MODEL
```

## Top-level fields

| Field | Default | Description |
|-------|---------|-------------|
| `description` | — | Human-readable agent description. Surfaced via the Hub MCP `list_agents` tool so other agents can discover this backend's capability. |
| `script` | — | Shorthand: a script path expanded into `command`/`args` by extension. An explicit `agentproc.command` always wins. |
| `transport` | `ilink` | IM protocol. Supported values: `ilink`, `telegram`, `wecom`, `feishu`, `discord`. Any unrecognised string loads a `NullTransport` placeholder (needs `--allow-null-transport`). |
| `im_credentials` | `{}` | IM-transport-specific credentials. Values support `${VAR}` environment-variable expansion at load time. Required keys vary by transport — see the [IM platform guides](/guide/telegram). |
| `via` | `hub` | Credential/connection mode (`ilink` only). `hub` resolves a virtual token via the Hub; `direct` connects to the real iLink upstream. |
| `base_url` | — | Real iLink upstream URL for `via: direct` (e.g. `https://ilinkai.weixin.qq.com`). Overrides `--hub-url`/`WEIXIN_BASE_URL` for this profile. Ignored when `via: hub`. |
| `agentproc` | — | The pure agentproc profile block (see below). |

## The `agentproc:` block

Field-for-field the [agentproc profile spec](https://agentproc.dev/spec/):

| Field | Default | Description |
|-------|---------|-------------|
| `executor` | — | In-process executor name (`claude-code`, `codex`, `cursor`, `codebuddy`, `agy`, …). When set and recognised, the runner drives the CLI directly; `command` may be empty. |
| `command` | — | `argv[0]` — a single token, never shell-split. Required when no `executor` and no `script`. |
| `args` | `[]` | `argv[1..]`. Supports `{{PROFILE_DIR}}`, `{{SESSION_ID}}`, `{{SESSION_NAME}}` placeholders (no shell). `{{MESSAGE}}` is **not** a placeholder — the message travels via stdin. |
| `cwd` | process cwd | Working dir for the CLI. Relative paths resolve against `{{PROFILE_DIR}}`; `~` and `$HOME` are expanded. |
| `env` | `{}` | Env vars for the CLI. `${VAR}` references expand against the bridge environment. |
| `env_allowlist` | — | When set, a `${VAR}` not in the list expands to empty with a stderr warning (POSIX-style). Absent = expand against the full environment (profiles are trusted input). |
| `timeout_secs` | `1800` | CLI stdout-read timeout. Worst-case total is `timeout_secs + 10s` (extra `child.wait()` after stdout EOF). |
| `kill_grace_secs` | `5` | SIGTERM → SIGKILL grace period. |
| `max_reply_chars` | `8000` | Reply body cap before truncation. |
| `truncation_suffix` | `"\n\n…(输出已截断)"` | Appended when the reply is truncated. |
| `streaming` | `true` | Forward `{"type":"partial"}` chunks in real time. `false` → only the final `{"type":"result"}` is sent. |
| `permission` | `false` | Enable the agentproc 0.4 tool-permission channel. `true` keeps stdin open for `permission_request`/`permission_response`. The bridge auto-approves every request (no per-profile policy). |
| `include_stderr_in_reply` | `false` | Include CLI stderr in the reply. |
| `send_error_reply` | `true` | Surface CLI failures as a reply to the user. |

## `script:` shorthand

Set `script: <path>` and the bridge infers the runtime from the extension:

| Extension | Inferred runtime |
|-----------|-------------------|
| `.py` | `python3 <script>` |
| `.js` / `.mjs` / `.cjs` | `node <script>` |
| `.ts` | `npx tsx <script>` |
| `.sh` / `.bash` | `bash <script>` |
| `.rb` | `ruby <script>` |
| other / none | execute directly (must be `chmod +x`) |

An explicit `agentproc.command` always wins; `script` is then informational only.

## Security: shell-`-c` + `{{MESSAGE}}` is rejected

`tokio::process::Command` does not invoke a shell, so user input is only dangerous when you explicitly invoke a shell with `-c` **and** interpolate the message. The bridge rejects this combo at load time:

```yaml
# REJECTED at load — arbitrary command execution risk
agentproc:
  command: bash
  args: ["-c", "echo {{MESSAGE}}"]
```

The message always travels via the **stdin turn object**, never via argv. Combined short options that include `c` (`-lc`, `-ic`, `-xc`) are also rejected; long options like `--color` are not. Shell running a script file (no `-c`) is fine.

## Environment variables (CLI / process)

These are read by the `im-agentproc` binary, not the profile:

| Variable | Purpose |
|----------|---------|
| `WEIXIN_BASE_URL` | Hub base URL (same key other backends use). Default `http://127.0.0.1:8765`. |
| `WEIXIN_TOKEN` | Explicit virtual token (skips auto-register / saved creds). |
| `ILINKHUB_BRIDGE_CREDS` | Override the credential JSON path. |
| `ILINKHUB_BRIDGE_REGISTER_NAME` | Stable client name when auto-registering. Default `local-<hostname>-<config-stem>`. |
| `ILINKHUB_BRIDGE_NON_INTERACTIVE` | Disable QR flows; `via: direct` bails instead of printing a QR when stdout is not a TTY. Injected by the manager into children. |
| `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT` | Allow a non-`ilink` transport's placeholder adapter. |
| `ILINKHUB_BRIDGE_DUMP_MSG` | `1`/`true`/`yes` → dump every inbound `WeixinMessage` JSON + `item_list[*].extra` to stderr. |
| `ILINK_ADMIN_TOKEN` | Hub admin auth token; must match the Hub when it enforces admin auth. Propagates to manager children. |
| `RUST_LOG` / `im_agentproc=info` | Tracing filter. |

::: warning Deprecated env vars
`ILINK_HUB_ADDR` and `ILINK_HUB_URL` are deprecated; migrate to `WEIXIN_BASE_URL`. The bridge warns at startup if the old vars are set without the new one.
:::

## File locations

| Path | Contents |
|------|----------|
| `~/.ilink-hub/ilink-hub-bridge.yaml` | Default bridge config (default mode). |
| `~/.ilink-hub/bridge-credentials.json` | Saved Hub virtual token (`via: hub`). |
| `~/.ilink-hub/direct-credentials.json` | Saved direct-upstream token (`via: direct`) — kept separate so switching modes does not clobber the other. |
| `~/.ilink-hub-bridge/profiles/` | Manager profiles directory. |
| `~/.ilink-hub-bridge/credentials/` | Manager per-profile credential JSONs. |

Next: [CLI reference](/cli) →
