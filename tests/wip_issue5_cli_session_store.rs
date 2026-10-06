//! issue #5 契约回归：非 iLink 通道的 CLI 会话续接错位。
//!
//! 状态：**已修复**（本文件常驻为回归；修复前的三态见下）。
//!
//! 错位三态（`src/bridge/transport/`，修复前）：
//! - `wecom.rs` / `feishu.rs` / `discord.rs` 把 IM 会话 id（chatid / chat_id /
//!   channel_id）直接填进 `InboundMessage.session_id`，dispatcher 当作 CLI 会话号
//!   交给 agentproc，claude-code executor 拼成 `--resume <chat_id>`
//!   （agentproc 0.11.1 `executors.rs:414-416,439-441`），每轮 resume 失败。
//! - `telegram.rs` 恒填 `None`，老会话永不续接。
//! - 四个适配器都丢弃回包里的 `cli_session_id`，没有 `(transport, 会话) →
//!   cli_session_id` 的本地 store。
//!
//! 修复形态：
//! - telegram / feishu / discord：`send_reply` 把回包的 `cli_session_id` 写进
//!   **进程内**的 `session_store::SessionStore`（键 `(transport, context_token)`），
//!   入站转换从 store 读回填 `session_id`（首轮 `None`）。
//! - wecom：`context_token` 是**每条消息一个**的 `req_id`（回复必须逐字回填，
//!   MCP 出站也用它），所以 store 键用 `chatid`，适配器内维护 `req_id → chatid`。
//! - store 只活在本进程：bridge 重启后非 iLink 通道回到冷会话；iLink 的续接仍由
//!   Hub 持久化（`ilink.rs` 未改）。
//!
//! 文件名的 `wip_` 前缀沿用本仓约定（cargo 不接受文件名含字面 `[wip]`，
//! 见 `tests/wip_issue10_degraded_cli_session.rs`）。
//!
//! 三类断言：
//! 1. **行为断言（telegram）**：`TelegramTransport::with_base_url` 是公开构造器，
//!    mockito 假 Telegram API → 走 `next_inbound` / `send_reply` 公开 seam，
//!    断言「回包写入 cli_session_id → 下一轮读回」。这是 issue #5 要求的
//!    「写入→下一轮读回」单测的可外部验证部分。
//! 2. **契约存在性（四个适配器）**：wecom/feishu/discord 的入站转换函数与
//!    WebSocket worker 都是 `pub(crate)`，集成测试无法直接调用；沿用
//!    `tests/wip_issue10_*.rs` 的源码字符串断言。真正的行为单测（fake inbound
//!    fixture → 断言 session_id == store 值）只能落在 crate 内
//!    `src/bridge/transport/*.rs` 的 `#[cfg(test)]`。
//! 3. **跨轮 e2e（telegram）**：真跑 `run_bridge_with_shutdown`，断言第 2 轮的
//!    agentproc 调用参数（`{{SESSION_ID}}`）确实是第 1 轮持久化的会话号。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use im_agentproc::bridge::config::BridgeApp;
use im_agentproc::bridge::transport::{
    InboundMessage, InboundOutcome, OutboundReply, SendOutcome, TelegramTransport, Transport,
    TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeStop};
use mockito::Matcher;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const TG_TOKEN: &str = "123:abc";

/// 每次运行取一个全新 chat_id：即便实现把 store 落到文件（跨进程、跨运行共享），
/// 本文件的用例也不会读到上一次运行残留的会话号。
fn fresh_chat_id(slot: i64) -> i64 {
    9_000_000_000 + (std::process::id() as i64 % 100_000) * 10 + slot
}

fn updates_body(update_id: i64, chat_id: i64, text: &str) -> String {
    json!({
        "ok": true,
        "result": [{
            "update_id": update_id,
            "message": {
                "message_id": update_id,
                "chat": { "id": chat_id, "type": "private", "first_name": "Tester" },
                "from": { "id": 4242, "first_name": "Tester", "is_bot": false },
                "text": text
            }
        }]
    })
    .to_string()
}

async fn mock_get_updates(
    server: &mut mockito::ServerGuard,
    offset: i64,
    update_id: i64,
    chat_id: i64,
    text: &str,
) -> mockito::Mock {
    server
        .mock("POST", "/bot123:abc/getUpdates")
        .match_body(Matcher::PartialJson(json!({ "offset": offset })))
        .with_status(200)
        .with_body(updates_body(update_id, chat_id, text))
        .create_async()
        .await
}

fn transport_for(server: &mockito::ServerGuard) -> TelegramTransport {
    TelegramTransport::with_base_url(TG_TOKEN.to_string(), server.url()).expect("test transport")
}

async fn poll_one(t: &TelegramTransport, buf: &mut String) -> InboundMessage {
    match t.next_inbound(buf).await.expect("next_inbound ok") {
        InboundOutcome::Messages(mut msgs) => msgs.pop().expect("one inbound message"),
        InboundOutcome::TokenRejected => panic!("unexpected TokenRejected"),
    }
}

// ── 1. telegram：写入 → 下一轮读回 ──────────────────────────────────────────

#[tokio::test]
async fn telegram_first_turn_has_no_resume_id() {
    let mut server = mockito::Server::new_async().await;
    let chat_id = fresh_chat_id(1);
    mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    let t = transport_for(&server);

    let mut buf = String::new();
    let turn1 = poll_one(&t, &mut buf).await;
    assert_eq!(
        turn1.session_id, None,
        "首轮没有可续接的 CLI 会话，session_id 必须是 None（而不是 chat_id）"
    );
}

#[tokio::test]
async fn telegram_send_reply_persists_cli_session_for_the_next_turn() {
    let mut server = mockito::Server::new_async().await;
    let chat_id = fresh_chat_id(2);
    let first = mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    let send = server
        .mock("POST", "/bot123:abc/sendMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .create_async()
        .await;
    let second = mock_get_updates(&mut server, 2, 2, chat_id, "again").await;
    let t = transport_for(&server);

    let mut buf = String::new();
    let turn1 = poll_one(&t, &mut buf).await;
    assert_eq!(
        turn1.context_token.as_deref(),
        Some(chat_id.to_string().as_str())
    );

    let outcome = t
        .send_reply(OutboundReply {
            context_token: chat_id.to_string(),
            text: "reply".into(),
            cli_session_id: Some("cli-7".into()),
            ..Default::default()
        })
        .await
        .expect("send_reply ok");
    assert_eq!(outcome, SendOutcome::Sent);

    let turn2 = poll_one(&t, &mut buf).await;
    assert_eq!(
        turn2.session_id.as_deref(),
        Some("cli-7"),
        "回包里的 cli_session_id 必须落进本地 store，并在同一会话的下一轮作为 \
         resume session id 读回；否则 telegram 每轮都是冷会话"
    );

    first.assert_async().await;
    send.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn telegram_persist_only_reply_still_persists_cli_session() {
    // 流式回复时 handle.rs 会发一条 `text` 为空、只带 `cli_session_id` 的
    // 「仅持久化」回包（src/bridge/dispatcher/handle.rs:290-330）。空文本的提前
    // 返回不得吞掉这次写入，否则流式通道的多轮记忆整轮丢失。
    let mut server = mockito::Server::new_async().await;
    let chat_id = fresh_chat_id(3);
    mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    let second = mock_get_updates(&mut server, 2, 2, chat_id, "again").await;
    let t = transport_for(&server);

    let mut buf = String::new();
    let _turn1 = poll_one(&t, &mut buf).await;

    let outcome = t
        .send_reply(OutboundReply {
            context_token: chat_id.to_string(),
            text: String::new(),
            cli_session_id: Some("cli-9".into()),
            ..Default::default()
        })
        .await
        .expect("persist-only send_reply ok");
    assert_eq!(outcome, SendOutcome::Sent);

    let turn2 = poll_one(&t, &mut buf).await;
    assert_eq!(
        turn2.session_id.as_deref(),
        Some("cli-9"),
        "空文本的「仅持久化」回包同样要写 store（telegram.rs:459-461 的提前返回 \
         必须放在写入之后）"
    );
    second.assert_async().await;
}

#[tokio::test]
async fn telegram_empty_cli_session_id_does_not_clobber_stored_session() {
    let mut server = mockito::Server::new_async().await;
    let chat_id = fresh_chat_id(4);
    mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    server
        .mock("POST", "/bot123:abc/sendMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .create_async()
        .await;
    let second = mock_get_updates(&mut server, 2, 2, chat_id, "again").await;
    let t = transport_for(&server);

    let mut buf = String::new();
    let _turn1 = poll_one(&t, &mut buf).await;

    for sid in [Some("cli-7".to_string()), Some(String::new()), None] {
        t.send_reply(OutboundReply {
            context_token: chat_id.to_string(),
            text: "reply".into(),
            cli_session_id: sid,
            ..Default::default()
        })
        .await
        .expect("send_reply ok");
    }

    let turn2 = poll_one(&t, &mut buf).await;
    assert_eq!(
        turn2.session_id.as_deref(),
        Some("cli-7"),
        "空串 / None 的 cli_session_id 必须被忽略：不得写入，也不得覆盖已存的会话号"
    );
    second.assert_async().await;
}

// ── 2. 四个适配器的源码契约（crate 内单测的替代断言）──────────────────────

const TELEGRAM_SRC: &str = include_str!("../src/bridge/transport/telegram.rs");
const WECOM_SRC: &str = include_str!("../src/bridge/transport/wecom.rs");
const FEISHU_SRC: &str = include_str!("../src/bridge/transport/feishu.rs");
const DISCORD_SRC: &str = include_str!("../src/bridge/transport/discord.rs");

fn adapters() -> [(&'static str, &'static str); 4] {
    [
        ("telegram", TELEGRAM_SRC),
        ("wecom", WECOM_SRC),
        ("feishu", FEISHU_SRC),
        ("discord", DISCORD_SRC),
    ]
}

/// 折叠连续空白，让断言不受换行 / 缩进影响。
fn squeeze(src: &str) -> String {
    src.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// issue #5 点名的三处错位写法（telegram 的错法是恒 `None`，另见行为断言）。
fn miswired_patterns() -> [(&'static str, &'static str); 3] {
    [
        ("wecom", r#"let session_id = body .get("chatid")"#),
        ("feishu", "session_id: chat_id,"),
        ("discord", "session_id: Some(channel_id),"),
    ]
}

#[test]
fn adapters_do_not_use_the_im_conversation_id_as_cli_session_id() {
    for (name, pattern) in miswired_patterns() {
        let src = adapters()
            .into_iter()
            .find(|(n, _)| *n == name)
            .expect("adapter source")
            .1;
        assert!(
            !squeeze(src).contains(pattern),
            "{name} 仍把 IM 会话 id 当 CLI 会话号（`{pattern}`）：agentproc 会拼成 \
             `--resume <IM 会话 id>`，claude-code 每轮 resume 失败/报错；\
             session_id 必须只由本地 store 填充"
        );
    }
}

#[test]
fn adapters_fill_session_id_from_a_local_store() {
    // 断言只要求「适配器读 store」这一契约存在（命名自由，含 `store` 子串即可，
    // 例如 `SessionStore::get` / `session_store::lookup`）。真正的行为断言
    // （fixture → session_id == store 值 / 首轮为 None）需落在 crate 内单测。
    for (name, src) in adapters() {
        assert!(
            src.to_lowercase().contains("store"),
            "{name} 没有从本地 store 读取 `(transport, 会话) → cli_session_id`，\
             session_id 仍由适配器凭空编造（chat_id / None）"
        );
    }
}

#[test]
fn adapters_persist_cli_session_id_on_the_reply_path() {
    for (name, src) in adapters() {
        assert!(
            src.contains("cli_session_id"),
            "{name} 的 send_reply 丢弃了回包里的 `cli_session_id`，本地 store \
             永远拿不到会话号，下一轮无法续接"
        );
    }
}

// ── 3. telegram 跨轮 e2e：第 2 轮 agentproc 收到正确的 resume id ──────────────

/// profile 子进程把 agentproc **实际收到的** `{{SESSION_ID}}` 回显进正文
/// （agentproc 0.11.1 对 `command` / `args` 做占位符替换，同一个值也写进
/// stdin turn 对象）；结果事件固定声明 `session_id: "cli-1"`，于是第一轮之后
/// store 里有号。
const PROFILE_YAML: &str = r#"
agentproc:
  command: /bin/sh
  args:
    - -c
    - |
      printf '{"type":"result","text":"resume_input=%s","session_id":"cli-1"}\n' "$0"
    - "{{SESSION_ID}}"
  timeout_secs: 20
  streaming: false
"#;

/// 包一层真实 `TelegramTransport`，只做两件事：
/// - `next_inbound` 从第 2 次 poll 起先等「上一轮把 cli_session_id 写进 store 了」——
///   dispatcher 的 poll 循环在派发消息后**立刻**再次 poll（`dispatcher/mod.rs:90-135`
///   不 sleep），不设门就可能在第 1 轮回包写入之前就把第 2 轮入站转换掉（真 flake）；
///   `Notify` 会保留一个 permit，写先于读也不会死锁。
/// - 记录每条入站 `session_id` 与每个回包正文，供断言。
struct GatedTg {
    inner: TelegramTransport,
    gate: Arc<Notify>,
    polls: AtomicUsize,
    inbound_sessions: Mutex<Vec<Option<String>>>,
    replies: Mutex<Vec<String>>,
}

impl GatedTg {
    fn new(inner: TelegramTransport) -> Self {
        Self {
            inner,
            gate: Arc::new(Notify::new()),
            polls: AtomicUsize::new(0),
            inbound_sessions: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
        }
    }

    fn inbound_sessions(&self) -> Vec<Option<String>> {
        self.inbound_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn replies(&self) -> Vec<String> {
        self.replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Transport for GatedTg {
    fn next_inbound<'a>(
        &'a self,
        buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            if self.polls.fetch_add(1, Ordering::SeqCst) > 0 {
                self.gate.notified().await;
            }
            let outcome = self.inner.next_inbound(buf).await?;
            if let InboundOutcome::Messages(msgs) = &outcome {
                let mut sessions = self
                    .inbound_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for msg in msgs {
                    sessions.push(msg.session_id.clone());
                }
            }
            Ok(outcome)
        })
    }

    fn send_reply<'a>(
        &'a self,
        reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
        Box::pin(async move {
            let persists = reply
                .cli_session_id
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            self.replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(reply.text.clone());
            let outcome = self.inner.send_reply(reply).await?;
            if persists {
                self.gate.notify_one();
            }
            Ok(outcome)
        })
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> TransportCapabilities {
        self.inner.capabilities()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telegram_second_turn_resumes_the_persisted_cli_session() {
    let mut server = mockito::Server::new_async().await;
    let chat_id = fresh_chat_id(5);
    // 第 1 轮（offset 0 → update_id 1），随后 buf 前进到 2。
    let first = mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    let send = server
        .mock("POST", "/bot123:abc/sendMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .expect(2)
        .create_async()
        .await;
    // 第 2 轮（offset 2 → update_id 3，同一 chat_id），随后 buf 前进到 4。
    let second = mock_get_updates(&mut server, 2, 3, chat_id, "again").await;
    let reject = server
        .mock("POST", "/bot123:abc/getUpdates")
        .match_body(Matcher::PartialJson(json!({ "offset": 4 })))
        .with_status(401)
        .with_body(r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#)
        .create_async()
        .await;

    let t = Arc::new(GatedTg::new(transport_for(&server)));
    let app = BridgeApp::parse_yaml(PROFILE_YAML, "issue5-e2e".to_string()).expect("profile yaml");

    let stop = tokio::time::timeout(
        Duration::from_secs(60),
        run_bridge_with_shutdown(t.clone(), app, CancellationToken::new()),
    )
    .await
    .expect("bridge did not stop within 60s (gate deadlock?)");

    first.assert_async().await;
    send.assert_async().await;
    second.assert_async().await;
    reject.assert_async().await;
    assert_eq!(
        stop,
        BridgeStop::TokenRejected,
        "第 3 次 poll 的 401 应让 bridge 收尾"
    );

    let sessions = t.inbound_sessions();
    assert_eq!(sessions.len(), 2, "应有两轮入站，实际 {sessions:?}");
    assert_eq!(
        sessions[0], None,
        "第 1 轮没有可续接的 CLI 会话（chat_id 只用于路由）"
    );
    assert_eq!(
        sessions[1].as_deref(),
        Some("cli-1"),
        "第 2 轮必须携带第 1 轮持久化的 cli_session_id 作为 resume id"
    );

    let replies = t.replies();
    assert_eq!(replies.len(), 2, "每轮一个回包，实际 {replies:?}");
    assert_eq!(
        replies[0], "resume_input=",
        "第 1 轮 agentproc 收到的 session id 必须为空"
    );
    assert_eq!(
        replies[1], "resume_input=cli-1",
        "第 2 轮 agentproc 收到的 session id 必须是第 1 轮持久化的 cli-1"
    );
}
