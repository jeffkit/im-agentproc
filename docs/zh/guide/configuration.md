# 配置参考

bridge profile 是**一个 YAML 文件 == 一个 agentproc profile**，采用 spec 对齐的 hub 形式：`agentproc:` 下是纯 agentproc 执行配置，外面是几个 ilink-hub 兄弟字段（`description`、`script`、`transport`、`via`、`base_url`）。

## 完整示例

```yaml
description: issue-keeper on MiniMax          # 经 Hub MCP list_agents 工具暴露
script: ./my-handler.py                       # 可选简写（展开成 command/args）
transport: ilink                              # 默认；当前只实现 ilink
via: hub                                      # 默认；或 `direct` 跳过 Hub
base_url: https://ilinkai.weixin.qq.com       # 仅 `via: direct` 用
agentproc:
  executor: claude-code                       # 可选 in-process executor
  command: python3                             # argv[0]——单个 token，永不切分
  args: ["./bridge.py"]                        # argv[1..]
  cwd: ~/projects/my-app                       # 工作目录；相对路径按 {{PROFILE_DIR}} 解析
  timeout_secs: 1800                           # 默认 1800（30 分钟）
  kill_grace_secs: 5                           # SIGTERM → SIGKILL 宽限
  max_reply_chars: 8000                        # 默认 8000
  truncation_suffix: "\n\n…(输出已截断)"
  streaming: true                              # 默认 true
  permission: false                            # 默认 false（agentproc 0.4 权限通道）
  include_stderr_in_reply: false
  send_error_reply: true                       # 默认 true
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}    # ${VAR} 运行时展开
    CLAUDE_MODEL: glm-5.2
  env_allowlist:                               # 限制哪些 ${VAR} 可展开
    - ANTHROPIC_API_KEY
    - CLAUDE_MODEL
```

## 顶层字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `description` | — | 人类可读的 agent 描述。经 Hub MCP `list_agents` 工具暴露，让其它 agent 能发现此后端能力。 |
| `script` | — | 简写：按扩展名展开成 `command`/`args` 的脚本路径。显式 `agentproc.command` 永远优先。 |
| `transport` | `ilink` | IM 协议。只实现了 `ilink`；其它任意字符串加载 `NullTransport` 占位（需 `--allow-null-transport`）。 |
| `via` | `hub` | 凭证/连接模式。`hub` 经 Hub 解析虚拟 token；`direct` 直连真实 iLink 上游。 |
| `base_url` | — | `via: direct` 的真实 iLink 上游 URL（如 `https://ilinkai.weixin.qq.com`）。覆盖 `--hub-url`/`WEIXIN_BASE_URL`。`via: hub` 时忽略。 |
| `agentproc` | — | 纯 agentproc profile 块（见下）。 |

## `agentproc:` 块

逐字段对应 [agentproc profile 规范](https://agentproc.dev/spec/)：

| 字段 | 默认 | 说明 |
|------|------|------|
| `executor` | — | in-process executor 名（`claude-code`、`codex`、`cursor`、`codebuddy`、`agy` 等）。设了且被识别时 runner 直接驱动 CLI，`command` 可空。 |
| `command` | — | `argv[0]`——单个 token，不做 shell 切分。无 `executor` 且无 `script` 时必填。 |
| `args` | `[]` | `argv[1..]`。支持 `{{PROFILE_DIR}}`、`{{SESSION_ID}}`、`{{SESSION_NAME}}` 占位符（不走 shell）。`{{MESSAGE}}` **不是**占位符——消息走 stdin。 |
| `cwd` | 进程 cwd | CLI 工作目录。相对路径按 `{{PROFILE_DIR}}` 解析；`~`、`$HOME` 会展开。 |
| `env` | `{}` | CLI 环境变量。`${VAR}` 按 bridge 环境展开。 |
| `env_allowlist` | — | 设了之后，不在列表里的 `${VAR}` 展开为空并打 stderr 警告（POSIX 风格）。不设 = 按完整环境展开（profile 是可信输入）。 |
| `timeout_secs` | `1800` | CLI stdout 读取超时。最坏总耗时 `timeout_secs + 10s`（stdout EOF 后还有 `child.wait()`）。 |
| `kill_grace_secs` | `5` | SIGTERM → SIGKILL 宽限期。 |
| `max_reply_chars` | `8000` | 回复正文截断上限。 |
| `truncation_suffix` | `"\n\n…(输出已截断)"` | 截断时追加的后缀。 |
| `streaming` | `true` | 实时转发 `{"type":"partial"}` 分片。`false` → 只发最终 `{"type":"result"}`。 |
| `permission` | `false` | 开启 agentproc 0.4 工具权限通道。`true` 时 stdin 保持打开，处理 `permission_request`/`permission_response`。bridge 对请求恒 allow（无 per-profile 策略）。 |
| `include_stderr_in_reply` | `false` | 把 CLI stderr 纳入回复。 |
| `send_error_reply` | `true` | 把 CLI 失败回复给用户。 |

## `script:` 简写

设 `script: <path>`，bridge 按扩展名推断运行时：

| 扩展名 | 推断运行时 |
|--------|------------|
| `.py` | `python3 <script>` |
| `.js` / `.mjs` / `.cjs` | `node <script>` |
| `.ts` | `npx tsx <script>` |
| `.sh` / `.bash` | `bash <script>` |
| `.rb` | `ruby <script>` |
| 其它 / 无 | 直接执行（需 `chmod +x`） |

显式 `agentproc.command` 永远优先；此时 `script` 仅作信息保留。

## 安全：shell-`-c` + `{{MESSAGE}}` 会被拒

`tokio::process::Command` 不走 shell，所以用户输入只有在显式用 shell `-c` **且**插值消息时才危险。bridge 在加载时即拒绝该组合：

```yaml
# 加载即拒——任意命令执行风险
agentproc:
  command: bash
  args: ["-c", "echo {{MESSAGE}}"]
```

消息始终经 **stdin turn 对象**传递，绝不走 argv。含 `c` 的组合短选项（`-lc`、`-ic`、`-xc`）同样被拒；长选项如 `--color` 不受影响。shell 跑脚本文件（无 `-c`）没问题。

## 环境变量（CLI / 进程）

这些由 `im-agentproc` 二进制读取，不是 profile：

| 变量 | 用途 |
|------|------|
| `WEIXIN_BASE_URL` | Hub base URL（与其它后端同键）。默认 `http://127.0.0.1:8765`。 |
| `WEIXIN_TOKEN` | 显式虚拟 token（跳过自动注册 / 保存凭证）。 |
| `ILINKHUB_BRIDGE_CREDS` | 覆盖凭证 JSON 路径。 |
| `ILINKHUB_BRIDGE_REGISTER_NAME` | 自动注册时的稳定 client 名。默认 `local-<hostname>-<config-stem>`。 |
| `ILINKHUB_BRIDGE_NON_INTERACTIVE` | 关闭扫码流程；stdout 非 TTY 时 `via: direct` 直接 bail 而非打印二维码。manager 注入给子进程。 |
| `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT` | 允许非 `ilink` transport 的占位适配器。 |
| `ILINKHUB_BRIDGE_DUMP_MSG` | `1`/`true`/`yes` → 把每条入站 `WeixinMessage` JSON + `item_list[*].extra` 打到 stderr。 |
| `ILINK_ADMIN_TOKEN` | Hub admin 鉴权 token；Hub 开 admin 鉴权时须与 Hub 一致。会传给 manager 子进程。 |
| `RUST_LOG` / `im_agentproc=info` | tracing 过滤。 |

::: warning 已弃用环境变量
`ILINK_HUB_ADDR` 和 `ILINK_HUB_URL` 已弃用，请迁移到 `WEIXIN_BASE_URL`。若旧变量已设而新变量未设，启动时会警告。
:::

## 文件位置

| 路径 | 内容 |
|------|------|
| `~/.ilink-hub/ilink-hub-bridge.yaml` | 默认 bridge 配置（默认模式）。 |
| `~/.ilink-hub/bridge-credentials.json` | 保存的 Hub 虚拟 token（`via: hub`）。 |
| `~/.ilink-hub/direct-credentials.json` | 保存的直连上游 token（`via: direct`）——单独存放，切换模式不互相覆盖。 |
| `~/.ilink-hub-bridge/profiles/` | manager profiles 目录。 |
| `~/.ilink-hub-bridge/credentials/` | manager 各 profile 凭证 JSON。 |

下一篇：[CLI 参考](/zh/cli) →
