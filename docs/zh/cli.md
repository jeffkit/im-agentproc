# CLI 参考

`im-agentproc` 二进制有三种运行模式：**默认**（一个 bridge，长轮询 Hub）、**`profile`**（以子进程跑内置 profile 处理器）、**`manager`**（监管多个子 bridge）。

```bash
im-agentproc [全局 flags] [子命令]
```

不带子命令时跑默认 bridge 模式。

## 全局 flags

| Flag | Env | 默认 | 说明 |
|------|-----|------|------|
| `--hub-url <url>` | `WEIXIN_BASE_URL` | `http://127.0.0.1:8765` | Hub base URL（与其它后端同键）。 |
| `--token <tok>` | `WEIXIN_TOKEN` | — | 显式虚拟 token。省略则用保存凭证、自动注册或 `--pair`。 |
| `--cred-file <path>` | `ILINKHUB_BRIDGE_CREDS` | 按 `via:` 默认 | 本地凭证 JSON 路径。 |
| `--pair` | — | `false` | 忽略保存凭证，跑扫码配对（手机确认）。 |
| `--register-name <n>` | `ILINKHUB_BRIDGE_REGISTER_NAME` | `local-<host>-<stem>` | 自动注册时的稳定 client 名。 |
| `--force-register` | — | `false` | 凭证文件存在但无效/为空时，删掉重新自动注册。 |
| `--allow-null-transport` | `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT` | `false` | 允许非 `ilink` transport 的占位适配器（可插拔冒烟测试）。 |
| `--no-interactive` | `ILINKHUB_BRIDGE_NON_INTERACTIVE` | `false` | 关闭扫码流程。stdout 非 TTY 时 `via: direct` 直接 bail。 |
| `--config <path>` | — | `~/.ilink-hub/ilink-hub-bridge.yaml` | bridge YAML 路径。仅默认模式。 |
| `--version` | — | — | 打印版本并退出。 |
| `--help`, `-h` | — | — | 显示帮助。 |

## 默认模式 —— `im-agentproc`

加载一个 bridge YAML，连 Hub，长轮询入站消息，每条消息跑 profile。

```bash
im-agentproc --config ~/.ilink-hub/ilink-hub-bridge.yaml
```

凭证解析顺序：

1. **显式 token** —— `--token` / `WEIXIN_TOKEN` 非空时。
2. **扫码配对** —— `--pair` 跑 Hub 扫码配对（手机确认）。
3. **零交互（默认）** —— 无 token 且无凭证文件时，进程调 `POST /hub/register`，把虚拟 token 写入本地 JSON。若凭证文件存在但无效/为空，**不会**静默覆盖（保护之前的扫码配对）——用 `--token`、`--pair` 或 `--force-register`。

Hub 开 admin 鉴权（`ILINK_ADMIN_TOKEN`）时，启动 bridge 前在环境里设上同名变量。

### 运行时停止原因

| `BridgeStop` | 含义 |
|--------------|------|
| `Shutdown` | 优雅关闭（Ctrl-C / SIGTERM）。 |
| `TokenRejected`（显式 token） | token 被拒；带提示 bail，要求重新注册或重新扫码。 |
| `TokenRejected`（保存凭证） | 运行时 token 被吊销；删凭证文件并重连重新注册。 |
| `FatalCliError` | CLI 自己的鉴权失败（如 `claude` 登出）。需人工处理并重启 bridge。 |

## `im-agentproc profile <type>`

以子进程跑一个**内置** profile 处理器。不连 Hub——从 stdin 读 agentproc turn 对象，向 stdout 写 NDJSON 事件，和任何外部 agentproc agent 一样。包装内置 CLI 的 profile 把 `im-agentproc profile <type>` 当 `command` 跑：

```yaml
agentproc:
  command: im-agentproc
  args: ["profile", "recursive"]
  cwd: ~/projects/recursive
```

### 内置类型

| `<type>` | CLI 工具 | 会话续接 | 备注 |
|----------|---------|----------|------|
| `claude-code` | `claude` | ✓（`--resume`） | Anthropic Claude Code；支持多模态 + 权限通道 |
| `codebuddy-code` | `codebuddy` | ✓（`--resume`） | CodeBuddy Code（stream-json 兼容） |
| `codex` | `codex` | ✗ | OpenAI Codex CLI |
| `cursor` | `cursor` | ✓（可选） | Cursor background agent CLI |
| `agy` | `agy` | ✓（`--conversation`） | Google Antigravity CLI |
| `recursive` | `recursive` | ✓（`-r`） | Recursive agent CLI（session UUID 从 stderr 读） |

每个处理器：从 stdin 读 turn → 跑 CLI（有 session 则续接，续接失败回退新会话）→ 流式 `partial` 事件 → 发一个终止 `result`（可选带 `session_id`）。见[内置 profile 规范](/zh/bridge/profile-spec)。

## `im-agentproc manager`

发现目录里的 profile YAML 文件，每个文件监管一个子 bridge。每个子进程按文件名取稳定 workspace/注册名，存独立凭证 JSON，所以每个子进程注册为独立 Hub 后端。

```bash
im-agentproc manager \
  --profiles-dir ~/.ilink-hub-bridge/profiles \
  --credentials-dir ~/.ilink-hub-bridge/credentials \
  --scan-interval-secs 5 \
  --restart-backoff-secs 5 \
  --max-restart-backoff-secs 60
```

| Flag | 默认 | 说明 |
|------|------|------|
| `--profiles-dir <dir>` | `~/.ilink-hub-bridge/profiles` | bridge profile YAML 目录（`*.yaml` / `*.yml`）。 |
| `--credentials-dir <dir>` | `~/.ilink-hub-bridge/credentials` | 各 profile 凭证 JSON 目录。 |
| `--scan-interval-secs <n>` | `5` | 扫描 profile 目录的间隔秒数。 |
| `--restart-backoff-secs <n>` | `5` | 退出的子进程重启前最小秒数。 |
| `--max-restart-backoff-secs <n>` | `60` | 指数退避重启的上限秒数。 |

manager 忽略 `--token`、`--cred-file`、`--register-name`、`--pair`（每个子进程各自自动注册）。它给子进程注入 `ILINKHUB_BRIDGE_NON_INTERACTIVE`，让它们 headless 下快速失败而非扫码阻塞；并传播 `ILINK_ADMIN_TOKEN`，让子进程在 Hub 开 admin 鉴权时也能自动注册。

::: warning 切勿在子进程间共享 vtoken
若 `ILINK_ADMIN_TOKEN` 缺失且 Hub 开了 admin 鉴权，子进程自动注册会失败（HTTP 401）。**不要**用复用其它后端凭证的方式绕过——共享 vtoken 会让多个 bridge 争抢同一消息队列（脑裂）。请设 `ILINK_ADMIN_TOKEN`。
:::

## 调试

```bash
# 把每条入站 WeixinMessage JSON + item_list[*].extra 打到 stderr
ILINKHUB_BRIDGE_DUMP_MSG=1 im-agentproc

# 调高 tracing
RUST_LOG=im_agentproc=debug im-agentproc
```
