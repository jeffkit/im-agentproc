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

dispatcher 永远看不到 IM 协议专属类型。`session_id` / `session_name` / `a2a_call_id` / `dispatch_key` 是 **bridge 运行时**字段，在 DTO 上一等公民，因为 dispatcher 需要它们做路由和 CLI 会话续接——由适配器填充：iLink-via-Hub 适配器从 `HubExt` 填；其它适配器从 `transport::session_store::SessionStore` 填（键为 `(transport, 适配器定义的会话键)`，首轮为空）。适配器**绝不能**把自己的 IM 会话 id（chat_id / channel_id）当 resume id 交给 CLI。`dispatch_key` 是适配器对「哪些消息属于同一个会话」的声明，dispatcher 只比较这个字符串。

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
| `InboundMessage` | `context_token`、`from_user`、`is_from_bot`、`text`、`media: Vec<MediaRef>`、`session_id`（CLI 续接号：iLink 取 `HubExt.session_id`；非 iLink 取本地 store，首轮为 `None`）、`session_name`、`dispatch_key`（稳定的会话路由键：dispatcher 把共享它的消息串行处理；WeCom 填 `wecom:{chatid}`，其它适配器留 `None`）、`a2a_call_id`、`extra`（IM 私有）、`raw`（完整原始 JSON，诊断用）。 |
| `OutboundReply` | `context_token`、`text`、`to_user`、`cli_session_id`、`session_name`、`a2a_call_id`、`usage`。 |
| `SendOutcome` | `Sent`、`Throttled { ret, errmsg }`（退避重试）、`Rejected { ret, errmsg }`（确定性拒绝——不重试，由 dispatcher 把该段降级为半长分片）。 |
| `TransportCapabilities` | `media_upload: bool`、`max_text_len: Option<usize>`（`None` = adapter 未声明客户端可见的通道上限，分片只受 profile 的 `max_reply_chars` 约束）。typing / 已读回执延后——Q5。 |

## 已实现适配器

| `transport:` | 状态 | 适配器 |
|--------------|------|--------|
| `ilink` | ✅ 已实现 | `IlinkTransport`——连 iLink（经 Hub 或 direct），长轮询入站，发送回复。 |
| 其它任意 | 🟡 占位 | `NullTransport`——构造成功（证明接缝能加载任意适配器），但每次 poll 返回"未实现"。需 `--allow-null-transport`，否则 bridge 启动时快速失败，避免误配的 transport 永久退避成僵尸。 |

## 新增一个 IM（飞书 / Telegram / …）

1. 在 `src/bridge/transport/` 下新子模块里为你的 IM **实现 `Transport`**。把你的 IM 入站 webhook/poll 事件翻译成 `InboundMessage`，把出站发送从 `OutboundReply` 翻译过去。
2. **填充 bridge 运行时字段**。`session_name` / `a2a_call_id` 取你 IM 自己的元数据；`session_id` 是 **CLI 续接号**，从 `transport::session_store::SessionStore` 读（`pub(crate)`，键为 `(transport, 你定义的会话键)`，如 chat_id），并在 `send_reply` 里把回包的 `cli_session_id` 写回 store——**必须放在任何提前返回之前**：流式回复的「仅持久化」回包正文为空但带 `cli_session_id`，放过了就会整轮丢号。绝不把自己的 IM 会话 id 当 resume id 交给 CLI（agentproc 会拼成 `--resume <id>`，CLI 每轮都失败）。store 按 profile 落盘（`<profile>.sessions.json`，与 profile YAML 同目录）——构造时从 `TransportBuildCtx::session_store_path` 拿路径——所以 bridge 重启后 CLI 会话仍在；文件缺失 / 损坏 / 不可写时只告警一次并退化为内存态。若你 IM 的回复 token 是逐条而非逐会话的（如 WeCom 的 `req_id`，必须逐字回填），store 键用稳定的会话 id，并在适配器内把回复 token 反查回会话键；同时把 `dispatch_key` 设为同一个稳定会话 id（WeCom 是 `wecom:{chatid}`），dispatcher 便会把同一会话的消息交给同一个 worker、按到达顺序处理，而不是每条回复 token 各起一个 worker。
3. 在 `build_transport`（`src/bin/im-agentproc.rs`）里**接工厂**，`transport:` 匹配你的 IM 名时构造你的适配器。
4. **声明能力**——仅当你的 IM 能为出站回复上传媒体时设 `media_upload: true`。
5. **IM 私有数据**放 `InboundMessage.extra`，别撑大主 DTO；`raw` 保留完整原始消息供诊断。

dispatcher、profile runner、会话处理、防循环、错误路径全部 IM 无关——你不应需要改它们。

## CLI 会话续接

非 iLink 通道靠三个 bridge 侧机制留在同一个 CLI 会话上：

- **落盘**：`SessionStore` 把 `(transport, 会话键) → cli_session_id` 写进 `<profile>.sessions.json`（profile YAML 的同目录兄弟文件，路径由 `paths::session_store_path_for_profile` 给出）。首次写入立即落盘，之后每秒最多一次（`Drop` 再补一次），并用同目录临时文件 + `rename` 原子替换。文件缺失 / 损坏 / 不可写时只告警一次：bridge 照常启动并走冷会话。MCP 出站子进程保持内存态（`SessionStore::new()`），两个进程不会写同一个文件。
- **串行化**：`dispatch_key` 相同的消息进同一个会话 worker，按到达顺序处理。WeCom 填 `wecom:{chatid}`；`context_token` 本身就逐会话稳定的适配器留 `None`，沿用 `context_token:session_name`。已知上限：每个键的队列是 `mpsc::channel(200)`，超出即按既有语义丢消息。
- **resume 失效回退**：带 `session_id` 的一轮若以「会话不存在」类错误失败（`bridge::is_stale_resume_error()`）且尚未把 partial 发给用户，dispatcher 会用空 session 重跑同一轮一次，而不是回复 CLI 错误；其它失败照原样上报。

## 为什么 `NullTransport` 快速失败

一个永远返回"未实现"的占位，否则会让 dispatcher 永久退避，看着像僵尸进程。所以 bridge 拒绝启动非 `ilink` transport，除非你传 `--allow-null-transport`（或设 `ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT=1`），让可插拔冒烟测试是显式的，而非意外误配。
