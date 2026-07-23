# Bridge 运行模式

bridge 是 IM-AgentProc 的核心：连 IM 传输、收入站消息、每条消息跑一个 agentproc profile。本页描述所有运行模式共有的运行时行为；YAML 字段见[配置参考](/zh/guide/configuration)，CLI flags 见[CLI 参考](/zh/cli)。

## 每条消息的循环

对每条入站文本消息，bridge：

1. 从 transport（默认 iLink 经 Hub）**读取**入站 IM 消息。
2. **解析** profile（一个 YAML == 一个 profile；单 profile 模式无路由前缀）。
3. **构造** agentproc turn 对象：`message`、`session_id`（从 Hub 的 `HubExt` 续接）、`session_name`、`attachments`、`permission`、`protocol_version`。
4. **运行** profile：
   - 设了 `executor:` 且被识别 → agentproc SDK **in-process** 驱动 CLI（无 bridge 子进程 fork）。
   - 否则 → spawn `command`/`args`（或 `script:` 简写），把 turn 写到其 stdin。
5. 把 `{"type":"partial"}` 分片实时经 Hub 转发回去（`streaming: true` 时）。
6. 从终止 `{"type":"result"}` 事件**发送**最终回复。
7. 在 Hub 上**持久化** CLI `session_id`（`HubExt.cli_session_id`），下一轮续接同一 CLI 会话。

## 会话续接

| 模式 | 跨消息续接？ | 怎么做 |
|------|-------------|--------|
| `via: hub` | ✓ | Hub 回显 `HubExt.session_id`；bridge 下一轮把它作为 `session_id` 传入。 |
| `via: direct` | ✗ | 真实上游不回显 Hub 的 `session_id`；每条消息起新 CLI 会话。 |

内置 profile 处理器还实现了**续接回退**：续接已有会话失败（过期/找不到）时，回退重试一次新会话，让用户仍能拿到回复而非裸错误。

## 防循环

入站消息若被标记为 bot/agent 产生（iLink `message_type == 2`），会被过滤掉——bridge 不会为自己的出站回复跑 profile，防止回复循环。

## 错误处理

- **CLI 鉴权失败**（`FatalCliError`）：CLI 自己的凭证过期（如 `claude` 登出）。bridge 停下并暴露原因；需人工重新登录 CLI 并重启 bridge。
- **token 被拒**（`TokenRejected`）：Hub 虚拟 token（或 direct bot_token）被吊销。显式 token 时带提示 bail；保存凭证时删凭证文件并重连重新注册。
- **profile 错误**：`send_error_reply: true`（默认）时，CLI 失败会回复给用户；否则只记日志。
- **超时**：SIGTERM → `kill_grace_secs`（默认 5s）→ SIGKILL。退出码 124。

## 启动探测

默认模式下，连 Hub 前 bridge 会对每个 profile 跑一次轻量探测，确认包装的 CLI 存在且可用。探测失败则立即退出，报 `Startup probe failed for profile <name>: <reason>`，而非第一条消息才静默失败。

## 权限通道（agentproc 0.4）

设 `permission: true` 开启可选的工具权限通道。bridge 写完 turn 对象后保持 agent 的 stdin 开着，翻译 agentproc `permission_request` / `permission_response` NDJSON 帧。bridge 对每个请求**恒 allow**——当前无 per-profile 策略。Claude Code 的 `--permission-prompt-tool stdio` 模式就是这样经 IM headless 驱动的。

## 优雅关闭

Ctrl-C 和 SIGTERM 取消一个共享 shutdown token。在飞的 AI 调用被优雅取消并通知用户。bridge 最多等 3s 让错误回复发出，再 abort 任务。

另见：[内置 profile 规范](/zh/bridge/profile-spec)、[Transport 扩展](/zh/transport)。
