# Transport 扩展

bridge 的核心 dispatcher 只认**通用 IM DTO**。每个具体 IM 协议（当前 iLink；未来飞书 / Telegram / …）是一个适配器，实现 `Transport` trait，在自己的 wire 类型与这些通用 DTO 之间翻译。这是让 bridge 支持多 IM、而 dispatcher 不依赖任何 IM wire 协议的接缝。

## 接缝

```
                 ┌──────────────────────────────────────────┐
   IM wire ──▶   │  Transport 适配器（iLink / 飞书 / …）      │ ──▶ 通用 InboundMessage
   ◀── IM wire  │                                          │ ◀── 通用 OutboundReply
                 └──────────────────────────────────────────┘
                                      │
                                      ▼
                 ┌──────────────────────────────────────────┐
                 │  Dispatcher（profile 运行、会话、防循环）  │
                 └──────────────────────────────────────────┘
```

dispatcher 永远看不到 IM 协议专属类型。`session_id` / `session_name` / `a2a_call_id` 是 **bridge 运行时**字段，在 DTO 上一等公民，因为 dispatcher 需要它们做路由和 CLI 会话续接——由适配器填充（iLink-via-Hub 适配器从 `HubExt` 填；其它 IM 从各自会话标识填）。

## `Transport` trait

object-safe、`Send` + `Sync`，返回 boxed future，让 dispatcher 能持有 `dyn Transport`：

```rust
pub trait Transport: Send + Sync {
    /// 拉下一批入站消息。用游标的 transport 原地更新 `buf`。
    fn next_inbound<'a>(
        &'a self,
        buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>>;

    /// 发一条回复。
    fn send_reply<'a>(
        &'a self,
        reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>>;

    /// 声明此 transport 的可选能力。
    fn capabilities(&self) -> TransportCapabilities;
}
```

| 类型 | 角色 |
|------|------|
| `InboundOutcome` | `Messages(Vec<InboundMessage>)` 或 `TokenRejected`（401/吊销 → 重新注册）。 |
| `InboundMessage` | `context_token`、`from_user`、`is_from_bot`、`text`、`media: Vec<MediaRef>`、`session_id`、`session_name`、`a2a_call_id`、`extra`（IM 私有）、`raw`（完整原始 JSON，诊断用）。 |
| `OutboundReply` | `context_token`、`text`、`to_user`、`cli_session_id`、`session_name`、`a2a_call_id`、`usage`。 |
| `SendOutcome` | `Sent` 或 `Throttled { ret, errmsg }`（退避重试）。 |
| `TransportCapabilities` | `media_upload: bool`（typing / 已读回执延后——Q5）。 |

## 已实现适配器

| `transport:` | 状态 | 适配器 |
|--------------|------|--------|
| `ilink` | ✅ 已实现 | `IlinkTransport`——连 iLink（经 Hub 或 direct），长轮询入站，发送回复。 |
| 其它任意 | 🟡 占位 | `NullTransport`——构造成功（证明接缝能加载任意适配器），但每次 poll 返回"未实现"。需 `--allow-null-transport`，否则 bridge 启动时快速失败，避免误配的 transport 永久退避成僵尸。 |

## 新增一个 IM（飞书 / Telegram / …）

1. 在 `src/bridge/transport/` 下新子模块里为你的 IM **实现 `Transport`**。把你的 IM 入站 webhook/poll 事件翻译成 `InboundMessage`，把出站发送从 `OutboundReply` 翻译过去。
2. 从你的 IM 会话标识**填充 bridge 运行时字段**（`session_id`、`session_name`、`a2a_call_id`），让 dispatcher 能路由和续接 CLI 会话。
3. 在 `build_transport`（`src/bin/im-agentproc.rs`）里**接工厂**，`transport:` 匹配你的 IM 名时构造你的适配器。
4. **声明能力**——仅当你的 IM 能为出站回复上传媒体时设 `media_upload: true`。
5. **IM 私有数据**放 `InboundMessage.extra`，别撑大主 DTO；`raw` 保留完整原始消息供诊断。

dispatcher、profile runner、会话处理、防循环、错误路径全部 IM 无关——你不应需要改它们。

## 为什么 `NullTransport` 快速失败

一个永远返回"未实现"的占位，否则会让 dispatcher 永久退避，看着像僵尸进程。所以 bridge 拒绝启动非 `ilink` transport，除非你传 `--allow-null-transport`（或设 `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT=1`），让可插拔冒烟测试是显式的，而非意外误配。
