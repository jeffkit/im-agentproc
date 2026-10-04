# Outbound delivery via MCP

This guide shows how to wire a hub profile (Claude Code / Codex / Codebuddy / …)
so the agent can send text and media back to the IM conversation through the
bridge's built-in [MCP](https://modelcontextprotocol.io/) server.

## Why MCP

The bridge's outbound path is exposed as four MCP tools
(`send_text` / `send_image` / `send_file` / `send_voice`) over a stdio
JSON-RPC 2.0 server. The agent calls a tool by name; the bridge translates
the call into the IM platform's native upload + send-message APIs.

MCP is the right seam because:

- Hub profiles that already speak MCP (Claude Code, Codex, Codebuddy, Agy,
  Aider, …) get outbound delivery **with zero profile-side code**.
- Outbound media types stay type-rich (image / audio / file / video) — the
  agent doesn't have to encode a file path inside a `text` field.
- The transport trait remains the source of truth for IM-specific quirks;
  the MCP layer is a thin façade that picks the right trait method.

## Profile wiring

Add an `mcp_servers` block to the profile YAML. The exact shape depends on
the CLI you wrap, but the bridge-side contract is: the profile CLI child
spawns `im-agentproc mcp-server` as a stdio sub-process, and the bridge
injects the per-turn conversation context into that child's env.

```yaml
# ~/.im-agentproc/telegram-claude.yaml
transport: telegram
im_credentials:
  token: ${TELEGRAM_BOT_TOKEN}

agentproc:
  executor: claude-code
  mcp_servers:
    - name: im-agentproc
      # The profile CLI child spawns this sub-process itself; it inherits
      # the env the bridge injected for this turn. Adjust the shape per SDK.
      command: im-agentproc
      args: ["mcp-server"]
```

`im-agentproc mcp-server` is the entry point that runs `crate::mcp::run_server`
against the transport named by `IM_AGENTPROC_MCP_TRANSPORT` and the
conversation context the bridge injected for the current turn.

## Per-turn context injection

Every inbound message runs the profile once, and for each run the bridge
dispatcher (`mcp_extra_env_for_profile()` in `bridge/dispatcher/handle.rs`)
assembles the `IM_AGENTPROC_MCP_*` map for *that* turn and hands it to
`agentproc::RunOptions::extra_env`. `extra_env` is the highest-priority config
layer, so it overrides same-named keys in the profile's `env:` block, and the
profile child inherits the result.

Two groups of keys:

**Conversation context — injected by the bridge; never export these by hand.**

| Key | Value for this turn |
|---|---|
| `IM_AGENTPROC_MCP_CONTEXT_TOKEN` | the inbound message's `context_token` (chat / channel / request id) |
| `IM_AGENTPROC_MCP_TO_USER` | the inbound message's `from_user` (key omitted when empty) |

A value exported once in the bridge process env is overwritten per turn — a
process-wide token would deliver every conversation's replies to the same chat,
so the dispatcher never falls back to it.

```sh
# Not needed, and actively wrong: the bridge sets these per turn.
# export IM_AGENTPROC_MCP_CONTEXT_TOKEN="$INBOUND_CONTEXT"
# export IM_AGENTPROC_MCP_TO_USER="$INBOUND_FROM_USER"
```

**Credentials — provided by the operator, from the bridge process env.**

```sh
# Bridge process (manager or foreground), once:
export IM_AGENTPROC_MCP_TRANSPORT=telegram
export IM_AGENTPROC_MCP_TELEGRAM_TOKEN="$TELEGRAM_BOT_TOKEN"
```

The same holds for `IM_AGENTPROC_MCP_FEISHU_APP_ID` / `_APP_SECRET`,
`IM_AGENTPROC_MCP_WECOM_BOT_ID` / `_BOT_SECRET`,
`IM_AGENTPROC_MCP_DISCORD_TOKEN` and `IM_AGENTPROC_MCP_ILINK_HUB_URL` /
`_ILINK_TOKEN`.

Because agentproc spawns a fresh CLI child per turn, each
`im-agentproc mcp-server` process sees exactly one turn's context — outbound
tool calls always land in the conversation that triggered the run.

## Tool catalogue

| Tool | Arguments | When to call |
|---|---|---|
| `send_text` | `text: string`, optional `reply_to: string` | Default reply path. Empty text is a silent no-op. |
| `send_image` | `source: {uri, name?}`, optional `caption: string`, `reply_to: string`, `as_document: boolean` | Send a screenshot, plot, generated image, … |
| `send_file` | `source: {uri, name?}`, optional `caption: string`, `reply_to: string` | Send a PDF, archive, doc, log dump, … |
| `send_voice` | `source: {uri, name?}`, optional `caption: string`, `reply_to: string` | Send a generated TTS clip or pre-recorded audio |

`source.uri` accepts three schemes:

- `file:///abs/path/to/file` — read locally.
- `https://cdn.example.com/x.png` — fetch server-side (the bridge's reqwest
  client, not the agent's).
- `data:image/png;base64,...` — base64 inline (for small images the agent
  generated in memory).

## Envelope semantics

- **Success**: `{ "content": [], "isError": false }` — silent. The agent
  doesn't read text on a successful send; it just continues.
- **Failure**: `{ "content": [{"type":"text","text":"<reason>"}], "isError": true }`
  — loud. The agent sees the reason and decides whether to retry.
- **Capability missing**: `send_image` etc. fail with
  `transport '<name>' does not support media upload` when the IM has
  `media_upload=false`. Don't retry — the call is structurally impossible
  on this bridge.

## Worked example: agent generates a chart and sends it

```text
[Claude Code]                         [im-agentproc]                 [Telegram]
  generate_chart tool                                                      
       │                                                                    
       ▼                                                                    
  writes /tmp/im-out/chart-1732.png                                        
       │                                                                    
       ▼                                                                    
  mcp.send_image({                                                          
    source: {uri: "file:///tmp/im-out/chart-1732.png", name: "chart.png"},
    caption: "Today's trend",
  })                                                                        
       │                                                                    
       │     ┌─ read_media_bytes(file://)                                  
       │     ├─ api.sendPhoto(chat_id, bytes, caption)  ──▶ Telegram API  
       │     └─ SendOutcome::Sent                                           
       │                                                                    
       ▼                                                                    
  { content: [], isError: false }  ─── silent success                      
```

The agent only sees the success envelope. The user sees the chart.

## Worked example: a voice clip that fails

```text
[Claude Code]    →   mcp.send_voice({ source: { uri: "data:audio/ogg;base64,..." } })
[im-agentproc]       │
                     ├─ transport capabilities.media_upload == true
                     ├─ read_media_bytes(data:) → bytes
                     └─ aibot_respond_msg({msgtype: "voice", voice: {media_base64}})
                     └─ SendOutcome::Sent
                     │
[Claude Code]   ←    { content: [], isError: false }
```

If `media_upload` were false:

```text
[Claude Code]    →   mcp.send_voice({ source: { uri: "data:audio/ogg;base64,..." } })
[im-agentproc]       │
                     ├─ capabilities.media_upload == false
                     └─ returns { content: [{type:"text", text:"transport 'telegram' does not support media upload"}], isError: true }
                     │
[Claude Code]   ←    { content: [...], isError: true }   ← agent sees the failure
```

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `unknown tool: send_image` from the CLI | The `mcp_servers` block isn't wired; the CLI never spawned the `im-agentproc mcp-server` sub-process |
| `transport '<name>' does not support media upload` | This IM adapter doesn't override `send_media`; pick a different IM or implement the override |
| `read local media file <path>: No such file` | `file://` URL points at a path the bridge can't reach (e.g. agent ran on a different host). Use `data:` or `https:` |
| HTTP 401 / 403 from the underlying IM API | The IM credentials (`im_credentials.*` or env fallback) are missing or expired |
| `throttled (code=429)` | The IM rate-limited the upload. The MCP server surfaces this as `isError: true` so the agent can back off |

## Reference

- MCP 2025-06-18 spec — [Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools), [Resources](https://modelcontextprotocol.io/specification/2025-06-18/server/resources) (not used by this server).
- `src/mcp/server.rs` — minimal JSON-RPC 2.0 framing on stdio.
- `src/mcp/tools.rs` — four tool handlers; one shared `OutboundDelivery`
  helper for the transport call.
- `docs/transport.md` — `Transport::send_media` contract and adapter-by-
  adapter upload paths.