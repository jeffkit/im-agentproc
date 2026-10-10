# Bridge run modes

The bridge is the core of IM-AgentProc: it connects an IM transport, receives inbound messages, and runs one agentproc profile per message. This page describes the runtime behavior shared by all run modes; for the YAML fields see [Configuration](/guide/configuration), and for the CLI flags see [CLI reference](/cli).

## The per-message loop

For every inbound text message, the bridge:

1. **Reads** the inbound IM message from the transport (iLink via Hub by default).
2. **Resolves** the profile (one YAML == one profile; no routing prefix in single-profile mode).
3. **Builds** an agentproc turn object: `message`, `session_id` (resumed from the Hub's `HubExt`), `session_name`, `attachments`, `permission`, `protocol_version`.
4. **Runs** the profile:
   - When `executor:` is set and recognised → the agentproc SDK drives the CLI **in-process** (no bridge subprocess fork).
   - Otherwise → spawns `command`/`args` (or the `script:` shorthand) and writes the turn to its stdin.
5. **Streams** `{"type":"partial"}` chunks back through the Hub as they arrive (when `streaming: true`).
6. **Sends** the final reply from the terminal `{"type":"result"}` event.
7. **Persists** the CLI `session_id` on the Hub (`HubExt.cli_session_id`) so the next turn resumes the same CLI session.

## Session continuity

| Mode | Resume across messages? | How |
|------|--------------------------|-----|
| `via: hub` | ✓ | The Hub echoes `HubExt.session_id`; the bridge passes it as `session_id` on the next turn. |
| `via: direct` | ✗ | The real upstream does not echo the Hub's `session_id`; each message starts a fresh CLI session. |

Built-in profile handlers also implement **resume-fallback**: if resuming an existing session fails (expired / not found), they retry once as a fresh session so the user still gets a response rather than a bare error.

## Anti-loop

Inbound messages flagged as produced by a bot/agent (iLink `message_type == 2`) are filtered out — the bridge will not run a profile for its own outgoing replies, preventing reply loops.

## Error handling

- **CLI auth failure** (`FatalCliError`): the CLI's own credentials expired (e.g. `claude` logged out). The bridge stops and surfaces the reason; a human must re-login to the CLI and restart the bridge.
- **Token rejected** (`TokenRejected`): the Hub virtual token (or direct bot_token) was revoked. With an explicit token the bridge bails with a hint; with saved creds it deletes the cred file and reconnects to re-register.
- **Profile errors**: when `send_error_reply: true` (default), CLI failures are surfaced as a reply to the user; otherwise they are logged only.
- **Timeout**: SIGTERM → `kill_grace_secs` (default 5s) → SIGKILL. Exit code 124.

## Startup probe

In default mode, before connecting to the Hub, the bridge runs a light probe on every profile to verify the wrapped CLI exists and is usable. If the probe fails, the bridge exits immediately with `Startup probe failed for profile <name>: <reason>` rather than silently failing on the first message.

## Permission channel (agentproc 0.4)

Set `permission: true` to enable the optional tool-permission channel. The bridge keeps the agent's stdin open after the turn object and translates agentproc `permission_request` / `permission_response` NDJSON frames. The bridge **auto-approves** every request — there is no per-profile policy today. This is how Claude Code's `--permission-prompt-tool stdio` mode is driven headlessly through an IM.

## Delivery guarantees

Taking a message off a transport is destructive — `getupdates` advances the Hub cursor before the reply is sent, Telegram confirms a batch only through the next poll's `offset` — so it used to double as an acknowledgement. The bridge now keeps a per-profile **write-ahead log** (`~/.ilink-hub-bridge/wal/<profile>/`, override `IM_AGENTPROC_WAL_DIR`):

- every inbound message is written (fsynced) there **before** it is dispatched;
- the entry is deleted only once the reply was confirmed delivered — an abandoned reply (retry budget exhausted, or shutdown cancelled the send) is logged and left in the log;
- on startup the bridge replays whatever is still there.

Delivery is therefore **at-least-once**: a message can be answered twice after a crash, but it is never silently dropped. A full session queue does not drop messages either — the overflow is parked and re-injected in arrival order once the worker drains.

## Graceful shutdown

Ctrl-C and SIGTERM cancel a shared shutdown token. In-flight AI calls are cancelled gracefully and users are notified. The bridge waits up to 3s for error replies to be sent before aborting the task.

See also: [Built-in profile spec](/bridge/profile-spec), [Transport extension](/transport).
