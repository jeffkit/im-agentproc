# 内置 Profile 规范

`im-agentproc profile <type>` 以子进程跑一个**内置** profile 处理器。本页记录每个内置（以及每个外部 profile 处理器）遵循的契约。它和 [agentproc P0 agent 规范](https://agentproc.dev/spec/) 是同一套——此处复述是因为源码引用了本文档。

## 契约

内置 profile 是一个 **agentproc 0.4 agent**。它：

1. 从 **stdin** 读取一条 NDJSON `{"type":"turn",...}` 对象（bridge 写入后 EOF——除非 `permission: true`，此时 stdin 保持打开）。
2. **运行**底层 CLI，`session_id` 非空时续接会话（续接失败回退新会话）。
3. 每来一个分片就向 **stdout** 流式写 `{"type":"partial","text":...}` 事件（每行一个 JSON 对象）。
4. 完成时发且仅发一个终止 `{"type":"result","text":...}` 事件（可选带 `session_id`）。
5. 失败时可发 `{"type":"error","message":...}`（非终止——处理器之后仍可返回 body；bridge 会丢弃随后的 result）。
6. 成功退出 `0`，失败非零。

消息**始终**经 stdin turn 对象传递——绝不走 argv 或 env。这就是 bridge 在加载时拒绝 `bash -c … {{MESSAGE}}` 的原因。

## Turn 对象（stdin）

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

| 字段 | 说明 |
|------|------|
| `message` | 用户消息文本（仅有附件时可空）。 |
| `session_id` | 上一轮 CLI session id 以续接；空 = 新会话。 |
| `session_name` | 人类可读会话名（分派 key）。 |
| `protocol_version` | wire 协议版本（`0.4`）。 |
| `attachments` | 媒体附件（`{kind, url, filename?, mime_type?, size?}`）。 |
| `permission` | bridge 开启权限通道时为 `true`。 |

## 输出事件（stdout）

每行一个 JSON 对象：

| `type` | 字段 | 何时 |
|--------|------|------|
| `partial` | `text`、`session_id?` | 一个流式分片。`streaming: true` 时实时转发给用户。 |
| `result` | `text`、`session_id?`、`usage?` | 终止成功。每轮至多一个。`session_id` 持久化到 Hub 供下一轮。 |
| `error` | `message`、`session_id?` | 失败。非终止——处理器仍可退出；bridge 把消息回复给用户。 |

`usage`（可选，在 `result` 上）：`input_tokens`、`output_tokens`、`total_tokens`、`cache_read_input_tokens`、`cache_creation_input_tokens`、`reasoning_tokens`、`duration_ms`、`cost_usd`。

## 内置类型

| `<type>` | CLI 工具 | 会话续接 | 备注 |
|----------|---------|----------|------|
| `claude-code` | `claude` | ✓（`--resume`） | 调 `claude --output-format stream-json [--resume <uuid>]`。多模态：下载图片/PDF 附件（图片 5MB / PDF 32MB 上限）作为 content block 转发。权限模式：`--permission-prompt-tool stdio`，翻译 Claude `control_request`/`control_response` ↔ agentproc `permission_request`/`permission_response`。 |
| `codebuddy-code` | `codebuddy` | ✓（`--resume`） | CodeBuddy Code，stream-json 兼容。 |
| `codex` | `codex` | ✗ | OpenAI Codex CLI（`@openai/codex`）。 |
| `cursor` | `cursor` | ✓（可选） | Cursor background agent CLI。 |
| `agy` | `agy` | ✓（`--conversation`） | Google Antigravity CLI。 |
| `recursive` | `recursive` | ✓（`-r`） | Recursive agent CLI；session UUID 从 stderr 读。 |

## 从 profile 用内置处理器

把 `im-agentproc profile <type>` 当 profile 的 `command` 跑：

```yaml
# hub 形式
agentproc:
  command: im-agentproc
  args: ["profile", "recursive"]
  cwd: ~/projects/recursive
  timeout_secs: 1800
```

或者 agentproc SDK 识别名字时用 in-process executor（无子进程 fork）：

```yaml
agentproc:
  executor: claude-code          # in-process；无需 `command`
  cwd: ~/projects/my-app
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

## 写自己的处理器

任何从 stdin 读 turn、向 stdout 写 NDJSON 的脚本都是合法 profile 处理器——不一定要内置。用 [agentproc SDK](https://agentproc.dev/sdk/)（Python / Node / Rust）省掉样板：

```python
# handler.py —— 一个自定义 agentproc agent
from agentproc import create_profile

async def handler(ctx):
    reply = await my_llm(ctx.message)
    return reply

create_profile(handler)
```

```yaml
# profile.yaml
script: ./handler.py             # 展开成 `python3 ./handler.py`
agentproc:
  timeout_secs: 600
```

完整 SDK API 见 [agentproc 文档](https://agentproc.dev)。
