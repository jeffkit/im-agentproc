# 什么是 IM-AgentProc？

IM-AgentProc 是 [agentproc](https://github.com/jeffkit/agentproc) 生态的 **IM 侧运行时**。它把 IM 传输桥接到本地编码 CLI（Claude Code、Codex、Cursor 等）——通过 agentproc profile：**一条入站 IM 文本消息 → 一次 agentproc profile 运行**。

## 为什么有它

IM-AgentProc 从 [`ilink-hub`](https://github.com/jeffkit/ilink-hub) 的 `src/bridge/` 子树抽离而来。把微信消息转成本地 CLI 运行的 bridge 原本住在 hub 里。当意识到同一套 bridge 逻辑可以服务不止一个 IM（飞书、Telegram 等）时，就把 bridge 拆成独立 crate，以便扩展多 IM 支持时不撑大 hub。

拆分理由和仓内剩余解耦工作见提案 `bridge-as-multi-im-runtime`（Appendix A）。

## 它不是什么

- **不是消息服务器。** `via: hub` 模式下它自己不持有微信连接——它注册为 iLink Hub 的 *后端*，长轮询 Hub 拿入站消息。（`via: direct` 直连真实 iLink 上游，但那是单 bridge 调试用，不是部署形态。）
- **不是编排器。** 每条消息跑一个 profile。多步流程、HITL、质量门在 [flowcast](https://github.com/jeffkit/flowcast) / [plaita](https://github.com/jeffkit/plaita) 里，它们自己也可以驱动 agentproc。
- **不是新协议。** profile 是纯 [agentproc P0](https://agentproc.dev/spec/)——`stdin` turn 对象、`stdout` NDJSON 事件。任何会 agentproc 的东西都能当 profile 处理器。

## 它在栈里的位置

```
微信用户
   │  （微信）
   ▼
iLink 官方 API
   │
   ▼
iLink Hub ── 把一个微信账号多路复用到多个后端
   │  （虚拟 token，长轮询）
   ▼
IM-AgentProc ── 每个 profile YAML 一个 bridge 进程
   │  （agentproc P0：stdin turn → stdout NDJSON）
   ▼
agentproc profile ── claude-code / codex / cursor / 你自己的脚本
```

`via: hub`（默认）模式下，IM-AgentProc 是 iLink Hub 的 *消费者*，不是它的兄弟。Hub 拥有唯一的微信连接；IM-AgentProc 是 Hub 把消息扇出到的多个后端之一。

## 两种连接模式

| `via:` | 连到 | 凭证 | 何时用 |
|--------|------|------|--------|
| `hub`（默认） | iLink Hub（`/hub/register` → 虚拟 token，或 `--pair` 扫码，或显式 `WEIXIN_TOKEN`） | 自动注册的 vtoken，存于 `~/.ilink-hub/bridge-credentials.json` | 正常运行——你已经在跑 iLink Hub |
| `direct` | 真实 iLink 上游（如 `https://ilinkai.weixin.qq.com`） | 显式 `WEIXIN_TOKEN`（预申请的 bot_token）或对上游扫码登录 | 不用 Hub 的单 bridge 调试。需要 `base_url:`（或非默认 `WEIXIN_BASE_URL`）；拒绝 localhost Hub 默认值，避免误连 Hub |

`via: direct` **不能跨消息续接 CLI 会话**——真实上游不回显 Hub 在 `via: hub` 下持久化的 `session_id`。每条消息起新 CLI 会话。

## 三种运行模式

| 命令 | 做什么 |
|------|--------|
| `im-agentproc`（默认） | 加载一个 bridge YAML，连 Hub，长轮询消息，每条消息跑 profile。 |
| `im-agentproc profile <type>` | 以子进程跑一个**内置** profile 处理器（P0 exec 协议：从 stdin 读 turn，向 stdout 写 NDJSON）。不连 Hub。供那些把 `im-agentproc profile <type>` 当 `command` 的 profile 用。 |
| `im-agentproc manager` | 扫描 profiles 目录；每个 `*.yaml` 监管一个子 bridge，各自注册为独立 Hub 后端。 |

每个 flag 见 [CLI 参考](/zh/cli)。

## 与 agentproc、hub 的关系

- **agentproc** 是共享协议 + SDK。IM-AgentProc 依赖 `agentproc` Rust crate（crates.io `0.11+`）拿 turn 对象、NDJSON 事件和 `run()` runner。
- **iLink Hub** 拥有微信连接并做多路复用。IM-AgentProc 是它的后端之一。
- **内置 profile**（`im-agentproc profile <type>`）本身也是 agentproc 0.4 *agent*：从 stdin 读 turn、向 stdout 写 NDJSON，和任何外部脚本或 SDK 处理器一模一样。

下一篇：[快速开始](/zh/guide/quickstart) →
