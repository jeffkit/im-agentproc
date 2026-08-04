---
name: agentproc-profile
description: >-
  Use this skill to create, edit, test, or publish an im-agentproc agentproc profile YAML,
  or to develop a custom profile handler using the agentproc SDK (Python / Node.js).
  Triggers on: "创建 agentproc profile", "新增 profile", "添加 profile", "配置 im-agentproc",
  "写一个 im-agentproc 配置", "发布 profile", "测试 profile", "bridge 配置",
  "用 Python/JS 写 profile", "自定义 profile", "im-agentproc profile", "agentproc profile",
  "create profile", "add new profile", "新增一个微信 AI 助手", "接 claude-code/cursor/codex/codebuddy".
version: 0.4.1
source: https://jeffkit.github.io/im-agentproc/skills/agentproc-profile/SKILL.md
---

# im-agentproc Bridge Profile 开发 Skill

本 skill 覆盖 bridge profile 的完整生命周期：**需求确认 → YAML 创建 → 测试 → 发布**，以及用 agentproc SDK（Python / Node.js）开发自定义 handler 的完整流程。

> **版本**：im-agentproc ≥ 0.1.1，agentproc 协议 0.4。
> **参考文档**：`docs/zh/guide/configuration.md`（字段完整参考）、`docs/zh/bridge/profile-spec.md`（协议规范）。

---

## 核心概念速查

| 术语 | 说明 |
|------|------|
| **Profile YAML** | 一个文件 = 一个 Hub 后端客户端，放入 `~/.ilink-hub-bridge/profiles/` 由 manager 自动发现 |
| **agentproc 0.4** | bridge ↔ handler 协议：stdin 写一行 NDJSON turn，stdout 逐行输出 NDJSON 事件 |
| **executor** | in-process 内置驱动（`claude-code`/`cursor`/`codex`/`codebuddy`/`agy`/`recursive`/`opencode`） |
| **script:** | 简写：按扩展名推断运行时（.py/.js/.ts/.sh/.rb） |
| **via: hub** | 经 Hub 注册虚拟 token（默认，支持跨消息 CLI 会话续接） |
| **via: direct** | 直连真实 iLink 上游（不支持续接） |

---

## Step 1：确认场景

向用户确认（未明确时询问）：

1. **Profile 名称**（文件名 stem，如 `my-claude` → `my-claude.yaml`，Hub 客户端名同此）
2. **用途**：内置 executor（claude-code/cursor/codebuddy 等）、自定义脚本 SDK，还是其他 CLI？
3. **项目目录** `cwd`：在哪个目录下运行 CLI？
4. **特殊 env**：API Key、BASE_URL、模型名等？**绝对不要写明文 key**——见 [Secrets](#secrets)

**快速路由：**
- 接 Claude Code → [内置 claude-code YAML](#yaml-claude-code)
- 接 Cursor / Codebuddy / Codex / Agy → [其他内置 executor](#yaml-builtin-other)
- 自定义 Python/JS 逻辑 → [SDK Handler 开发](#sdk-development)
- 自定义任意 CLI → [自定义 command YAML](#yaml-custom-cli)

---

## Step 2：生成 Profile YAML

### 发布路径

```
~/.ilink-hub-bridge/profiles/<name>.yaml
```

---

### 内置 claude-code（推荐） {#yaml-claude-code}

```yaml
# ~/.ilink-hub-bridge/profiles/<name>.yaml
agentproc:
  executor: claude-code        # in-process；自动管理 --resume 会话续接
  cwd: ~/projects/my-app       # CLI 工作目录
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}    # 引用 bridge 进程 env
    # CLAUDE_MODEL: claude-sonnet-4-5          # 可选：覆盖模型
```

支持流式输出（默认开启），每条消息续接同一 Claude 会话。

---

### 其他内置 executor {#yaml-builtin-other}

```yaml
# Cursor
agentproc:
  executor: cursor
  cwd: ~/projects/my-app

# Codebuddy
agentproc:
  executor: codebuddy
  command: codebuddy           # 显式命令（manager 模式须填）
  cwd: ~/projects/my-app
  env:
    CODEBUDDY_MODEL: hy3-ioa

# Codex
agentproc:
  executor: codex
  cwd: ~/projects/my-app
  env:
    OPENAI_API_KEY: ${OPENAI_API_KEY}

# Recursive
agentproc:
  executor: recursive
  cwd: ~/projects/my-app

# OpenCode
agentproc:
  executor: opencode
  cwd: ~/projects/my-app
```

---

### 自定义 SDK 脚本（Python/JS） {#yaml-script}

```yaml
description: 我的自定义 AI 助手
script: /path/to/handler.py     # bridge 按扩展名自动推断：python3 handler.py
agentproc:
  timeout_secs: 120
  max_reply_chars: 8000
  env:
    MY_API_KEY: ${MY_API_KEY}
```

`script:` 扩展名推断规则：

| 扩展名 | 推断运行时 |
|--------|-----------|
| `.py` | `python3 <script>` |
| `.js` / `.mjs` / `.cjs` | `node <script>` |
| `.ts` | `npx tsx <script>` |
| `.sh` / `.bash` | `bash <script>` |
| `.rb` | `ruby <script>` |
| 其他 | 直接执行（须 `chmod +x`） |

---

### 自定义 CLI {#yaml-custom-cli}

消息始终经 **stdin turn 对象** 传递，绝不走 argv（安全限制：`bash -c "... {{MESSAGE}}"` 会被 bridge 拒绝加载）。

```yaml
agentproc:
  command: my-cli              # argv[0]
  args: ["--flag", "value"]    # argv[1..]；支持 {{PROFILE_DIR}} / {{SESSION_ID}} / {{SESSION_NAME}}
  cwd: ~/projects/my-app
  timeout_secs: 300
  max_reply_chars: 8000
  streaming: false             # 若 CLI 不输出 partial 事件，关闭流式
```

---

### `agentproc:` 字段速查

| 字段 | 默认 | 说明 |
|------|------|------|
| `executor` | — | in-process executor（识别时无需 `command`） |
| `command` | — | `argv[0]`，单 token，不做 shell 切分 |
| `args` | `[]` | `argv[1..]`，支持 `{{PROFILE_DIR}}`/`{{SESSION_ID}}`/`{{SESSION_NAME}}` |
| `cwd` | 进程 cwd | 工作目录；`~`、`$HOME` 会展开 |
| `env` | `{}` | `${VAR}` 从 bridge 进程 env 展开 |
| `env_allowlist` | — | 设后只允许列表内变量展开 |
| `timeout_secs` | `1800` | CLI 超时（秒） |
| `kill_grace_secs` | `5` | SIGTERM → SIGKILL 宽限 |
| `max_reply_chars` | `8000` | 回复截断上限 |
| `streaming` | `true` | 实时转发 `partial` 事件 |
| `permission` | `false` | 开启 agentproc 0.4 工具权限通道 |
| `send_error_reply` | `true` | CLI 失败时回复给用户 |
| `include_stderr_in_reply` | `false` | 是否附加 stderr |

### 顶层字段

| 字段 | 说明 |
|------|------|
| `description` | 人类可读描述，经 Hub MCP `list_agents` 工具暴露 |
| `script` | 简写脚本路径，按扩展名推断运行时 |
| `via` | `hub`（默认）或 `direct` |
| `base_url` | `via: direct` 时的真实 iLink 上游 URL |

---

## Step 3：用 SDK 开发自定义 Handler {#sdk-development}

agentproc SDK 封装了 0.4 NDJSON 协议样板；handler 只需返回字符串或 `AgentResult`。

### Python SDK

```bash
pip install "agentproc>=0.9"
```

**最简 handler：**

```python
from agentproc import create_profile, AgentContext

async def handler(ctx: AgentContext) -> str:
    return f"你说的是：{ctx.message}"

create_profile(handler)
```

**接 Anthropic API（流式）：**

```python
import os
import anthropic
from agentproc import create_profile, AgentContext

client = anthropic.AsyncAnthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

async def handler(ctx: AgentContext) -> str:
    response = await client.messages.create(
        model="claude-sonnet-4-5",
        max_tokens=2048,
        messages=[{"role": "user", "content": ctx.message}],
    )
    return response.content[0].text

create_profile(handler)
```

**多轮对话（带历史）：**

```python
from agentproc import create_profile, AgentContext, AgentResult, load_history, append_history, HistoryEntry
from openai import AsyncOpenAI

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: AgentContext) -> AgentResult:
    history = load_history(ctx.session_id)
    messages = [
        {"role": "system", "content": "你是一个友好的 AI 助手。"},
        *[{"role": e.role, "content": e.content} for e in history],
        {"role": "user", "content": ctx.message},
    ]
    completion = await client.chat.completions.create(model="gpt-4o-mini", messages=messages)
    reply = completion.choices[0].message.content

    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])
    return AgentResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

### Node.js SDK

```bash
npm install agentproc
```

```js
const { createProfile } = require('agentproc');
const Anthropic = require('@anthropic-ai/sdk');

const client = new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY });

createProfile(async ({ message }) => {
  const response = await client.messages.create({
    model: 'claude-sonnet-4-5',
    max_tokens: 2048,
    messages: [{ role: 'user', content: message }],
  });
  return response.content[0].text;
});
```

---

## Step 4：测试

不启动完整 bridge，向 stdin 写一行 turn NDJSON 模拟调用：

```bash
TURN='{"type":"turn","message":"你好","session_id":"","protocol_version":"0.4","session_name":"default","attachments":[],"permission":false}'

# 测试内置 claude-code
echo "$TURN" | im-agentproc profile claude-code

# 测试自定义 Python handler
echo "$TURN" | python3 /path/to/handler.py

# 测试自定义 JS handler
echo "$TURN" | node /path/to/handler.js
```

**期望输出（NDJSON 事件流）：**
```
{"type":"partial","text":"..."} ← 流式分片（可多行）
{"type":"result","text":"完整回复","session_id":"<uuid>"}
```

退出码 0 = 成功；`{"type":"error","message":"..."}` = turn 失败。

---

## Step 5：发布

```bash
mkdir -p ~/.ilink-hub-bridge/profiles
cp /path/to/my-profile.yaml ~/.ilink-hub-bridge/profiles/<name>.yaml
```

manager 每 5 秒扫描一次 profiles 目录，自动发现新文件并启动子 bridge。

若 manager 未运行（或通过 launchd 管理）：

```bash
# 检查 launchd 状态
launchctl list com.ilink-hub.bridge-manager

# 若未注册则 bootstrap
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist
```

验证 manager 日志：
```
INFO im_agentproc::bridge::manager: started bridge child profile=<name> ...
INFO im_agentproc::bridge::dispatcher: im-agentproc connected; waiting for getupdates profiles=["<name>"]
```

---

## Step 6：微信中使用

```
/list              # 查看所有已注册 bridge 客户端
/use <name>        # 切换到此 bridge（名称 = YAML 文件 stem）
```

---

## Secrets & 环境变量 {#secrets}

**铁律：绝不把明文 API key / token 写进 profile YAML。**

`env:` 字段里的 `${VAR}` 会在加载时从 bridge **进程环境**展开（bridge 进程的 env 来自 launchd plist 的 `EnvironmentVariables`）。

```yaml
agentproc:
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}   # ✅ 引用 env
    ANTHROPIC_API_KEY: sk-ant-xxx              # ❌ 明文写死
```

限制展开范围（推荐）：

```yaml
agentproc:
  env_allowlist: [ANTHROPIC_API_KEY]
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

---

## 调试速查

```bash
# 查看 manager 实时日志
tail -f ~/ilink-logs/bridge-manager.log

# 查看已发布 profiles
ls ~/.ilink-hub-bridge/profiles/

# 查看凭证（每个 profile 独立）
ls ~/.ilink-hub-bridge/credentials/

# 强制重置凭证（token 失效时）
rm ~/.ilink-hub-bridge/credentials/<name>.json

# 开启消息 dump（调试入站消息）
ILINKHUB_BRIDGE_DUMP_MSG=1 im-agentproc manager
```

---

## 旧 YAML 迁移（从 0.3 格式）

旧格式使用 `profiles:` 多 profile map + `routing:` + `type:`，新格式**一文件一 profile**：

| 旧字段 | 新写法 |
|--------|--------|
| `profiles.xxx.type: claude-code` | `agentproc.executor: claude-code` |
| `profiles.xxx.command/args/cwd/env` | `agentproc.command/args/cwd/env` |
| `routing: {strategy: fixed}` | 删除（固定 = 单文件） |
| 多 profile 路由 | 拆为多个 YAML 文件，用 `/use <name>` 切换 |
| `skip_bot_messages` / `require_text` | 删除（内置恒启用） |
| 顶层 `send_error_reply` | 移入 `agentproc.send_error_reply` |
| `permission_default: ask` | 删除（permission 恒 allow） |
| `codebuddy-code` executor | 改为 `codebuddy` |
