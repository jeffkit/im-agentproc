# Built-in profile spec

`im-agentproc profile <type>` runs a **built-in** profile handler as a subprocess. This page documents the contract every built-in (and every external profile handler) follows. It is the same contract as the [agentproc P0 agent spec](https://agentproc.dev/spec/) — repeated here because the source code references this document.

## The contract

A built-in profile is an **agentproc 0.4 agent**. It:

1. **Reads** one NDJSON `{"type":"turn",...}` object from **stdin** (the bridge writes it, then EOF — unless `permission: true`, in which case stdin stays open).
2. **Runs** the underlying CLI, resuming the session when `session_id` is non-empty (falling back to a fresh session on resume failure).
3. **Streams** `{"type":"partial","text":...}` events on **stdout** as chunks arrive (one JSON object per line).
4. **Emits** exactly one terminal `{"type":"result","text":...}` event (with optional `session_id`) when done.
5. **May emit** `{"type":"error","message":...}` on failure (non-terminal — the handler may still return a body after; the bridge discards a subsequent result).
6. **Exits** `0` on success, non-zero on failure.

The message **always** travels via the stdin turn object — never via argv or env. This is why the bridge rejects `bash -c … {{MESSAGE}}` at load time.

## Turn object (stdin)

```json
{
  "type": "turn",
  "message": "explain this codebase",
  "session_id": "13c2f6ec-1f97-42c4-be9e-9475129e243c",
  "session_name": "default",
  "protocol_version": "0.4",
  "attachments": [
    { "kind": "image", "url": "https://...", "filename": "pic.png", "mime_type": "image/png" }
  ],
  "permission": false
}
```

| Field | Description |
|-------|-------------|
| `message` | User message text (may be empty when only attachments are present). |
| `session_id` | Previous CLI session id to resume; empty = new session. |
| `session_name` | Human-readable session name (dispatch key). |
| `protocol_version` | Wire protocol version (`0.4`). |
| `attachments` | Media attachments (`{kind, url, filename?, mime_type?, size?}`). |
| `permission` | `true` when the bridge enabled the permission channel. |

## Output events (stdout)

Each line is one JSON object:

| `type` | Fields | When |
|--------|--------|------|
| `partial` | `text`, `session_id?` | A streamed chunk. Forwarded to the user in real time when `streaming: true`. |
| `result` | `text`, `session_id?`, `usage?` | Terminal success. At most one per turn. `session_id` is persisted on the Hub for the next turn. |
| `error` | `message`, `session_id?` | Failure. Non-terminal — the handler may still exit; the bridge surfaces the message to the user. |

`usage` (optional, on `result`): `input_tokens`, `output_tokens`, `total_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `reasoning_tokens`, `duration_ms`, `cost_usd`.

## Built-in types

| `<type>` | CLI tool | Session resume | Notes |
|----------|---------|-----------------|-------|
| `claude-code` | `claude` | ✓ (`--resume`) | Calls `claude --output-format stream-json [--resume <uuid>]`. Multimodal: downloads image/PDF attachments (5MB image / 32MB PDF caps) and forwards as content blocks. Permission mode: `--permission-prompt-tool stdio`, translating Claude `control_request`/`control_response` ↔ agentproc `permission_request`/`permission_response`. |
| `codebuddy-code` | `codebuddy` | ✓ (`--resume`) | CodeBuddy Code, stream-json compatible. |
| `codex` | `codex` | ✗ | OpenAI Codex CLI (`@openai/codex`). |
| `cursor` | `cursor` | ✓ (optional) | Cursor background agent CLI. |
| `agy` | `agy` | ✓ (`--conversation`) | Google Antigravity CLI. |
| `recursive` | `recursive` | ✓ (`-r`) | Recursive agent CLI; session UUID read from stderr. |

## Using a built-in from a profile

Spawn `im-agentproc profile <type>` as the profile's `command`:

```yaml
# hub form
agentproc:
  command: im-agentproc
  args: ["profile", "recursive"]
  cwd: ~/projects/recursive
  timeout_secs: 1800
```

Or use the in-process executor instead (no subprocess fork) when the agentproc SDK recognises the name:

```yaml
agentproc:
  executor: claude-code          # in-process; no `command` needed
  cwd: ~/projects/my-app
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

## Writing your own handler

Any script that reads the turn from stdin and writes NDJSON on stdout is a valid profile handler — you do not need a built-in. Use the [agentproc SDK](https://agentproc.dev/sdk/) (Python / Node / Rust) to drop the boilerplate:

```python
# handler.py — a custom agentproc agent
from agentproc import create_profile

async def handler(ctx):
    reply = await my_llm(ctx.message)
    return reply

create_profile(handler)
```

```yaml
# profile.yaml
script: ./handler.py             # expanded to `python3 ./handler.py`
agentproc:
  timeout_secs: 600
```

See the [agentproc docs](https://agentproc.dev) for the full SDK API.
