# 通过 MCP 出站投递

本指南说明如何把 hub profile（Claude Code / Codex / Codebuddy / …）接到
bridge 内置的 [MCP](https://modelcontextprotocol.io/) server，让 agent
把文本和媒体送回 IM 会话。

## 为什么用 MCP

bridge 的出站路径通过 stdio JSON-RPC 2.0 server 暴露成四个 MCP 工具
（`send_text` / `send_image` / `send_file` / `send_voice`）。agent 按
名字调用工具，bridge 把它翻译成对应 IM 的原生上传 + 发消息 API。

选 MCP 作为接缝的原因是：

- 已经支持 MCP 的 hub profile（Claude Code、Codex、Codebuddy、Agy、
  Aider、…）**profile 侧零代码改动**就能拿到出站投递能力。
- 出站媒体类型保持类型丰富（image / audio / file / video）—— agent
  不用把文件路径塞进 `text` 字段。
- Transport trait 仍然是 IM 特定行为的唯一真相来源；MCP 层是挑
  trait 方法的薄壳。

## profile 配置

在 profile YAML 里加 `mcp_servers` 块。CLI 不同，shape 也不同，但
bridge 侧的契约是：bridge 自己跑一个 stdio MCP server，CLI 子进程
attach 上去。

```yaml
# ~/.im-agentproc/telegram-claude.yaml
transport: telegram
im_credentials:
  token: ${TELEGRAM_BOT_TOKEN}

agentproc:
  executor: claude-code
  mcp_servers:
    - name: im-agentproc
      # bridge 进程是父进程；CLI 子进程通过 build_transport 注入的
      # 环境变量找到它。按 SDK 调整。
      command: ${IM_AGENTPROC_MCP_BRIDGE}
      args: ["mcp-server"]
```

bridge 二进制的 `mcp-server` 子命令就是 `crate::mcp::run_server`
针对当前 bridge 的 transport 和入站会话上下文的入口。

## 工具清单

| 工具 | 参数 | 何时调用 |
|---|---|---|
| `send_text` | `text: string`，可选 `reply_to: string` | 默认回复路径。空文本是 silent no-op。 |
| `send_image` | `source: {uri, name?}`，可选 `caption: string`、`reply_to: string`、`as_document: boolean` | 发送截图、图表、生成图等。 |
| `send_file` | `source: {uri, name?}`，可选 `caption: string`、`reply_to: string` | 发送 PDF、归档、文档、日志 dump 等。 |
| `send_voice` | `source: {uri, name?}`，可选 `caption: string`、`reply_to: string` | 发送生成的 TTS 或预录音频。 |

`source.uri` 接受三种 scheme：

- `file:///abs/path/to/file` —— 本地读。
- `https://cdn.example.com/x.png` —— 由 bridge 的 reqwest 客户端在
  server 侧 fetch（不走 agent）。
- `data:image/png;base64,...` —— base64 内联（agent 内存里生成的小图）。

## 返回信封语义

- **成功**：`{ "content": [], "isError": false }` —— silent。agent
  不读文本，直接继续。
- **失败**：`{ "content": [{"type":"text","text":"<原因>"}], "isError": true }`
  —— loud。agent 看到原因，决定要不要重试。
- **能力缺失**：`send_image` 等在 `media_upload=false` 时返回
  `transport '<name>' does not support media upload`。**不要重试**——
  这个调用在这个 bridge 上结构性不可能。

## 例子：agent 生成图表并发出去

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
    caption: "今日趋势",
  })                                                                        
       │                                                                    
       │     ┌─ read_media_bytes(file://)                                  
       │     ├─ api.sendPhoto(chat_id, bytes, caption)  ──▶ Telegram API  
       │     └─ SendOutcome::Sent                                           
       │                                                                    
       ▼                                                                    
  { content: [], isError: false }  ─── silent success                      
```

agent 只看到 success 信封。用户看到图表。

## 例子：语音片段发送失败

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

如果 `media_upload` 是 false：

```text
[Claude Code]    →   mcp.send_voice({ source: { uri: "data:audio/ogg;base64,..." } })
[im-agentproc]       │
                     ├─ capabilities.media_upload == false
                     └─ returns { content: [{type:"text", text:"transport 'telegram' does not support media upload"}], isError: true }
                     │
[Claude Code]   ←    { content: [...], isError: true }   ← agent 看到失败
```

## 排错

| 现象 | 可能原因 |
|---|---|
| CLI 报 `unknown tool: send_image` | `mcp_servers` 块没接上，CLI 没连到 bridge 的 MCP server |
| `transport '<name>' does not support media upload` | 该 IM adapter 没 override `send_media`；换 IM 或自己实现 override |
| `read local media file <path>: No such file` | `file://` URL 指向 bridge 访问不到的路径（agent 跑在别的机器上）。改用 `data:` 或 `https:` |
| IM API 返回 HTTP 401 / 403 | IM 凭据（`im_credentials.*` 或环境变量回退）缺失或过期 |
| `throttled (code=429)` | IM 限流了上传。MCP server 以 `isError: true` 暴露，agent 可以退避 |

## 参考

- MCP 2025-06-18 spec —— [Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)、[Resources](https://modelcontextprotocol.io/specification/2025-06-18/server/resources)（本 server 不用）。
- `src/mcp/server.rs` —— stdio 上的最小 JSON-RPC 2.0 framing。
- `src/mcp/tools.rs` —— 四个 tool handler；共用 `OutboundDelivery` 调 transport。
- `docs/transport.md` —— `Transport::send_media` 契约 + 每个 adapter 的上传路径。