//! issue #15 子项 1 的定向回归：非 iLink 通道的 CLI 会话 store 落盘。
//!
//! #5 交付的 `SessionStore` 只活在进程里（bridge 重启 → 冷会话）。本文件用公开
//! API 验证修复后的契约：
//! 1. 第 1 次 `run_bridge_with_shutdown` 把回包的 `cli_session_id` 落盘；
//! 2. 用**同一 store 路径**重新构造 transport（= bridge 重启）后，下一轮入站
//!    `InboundMessage.session_id` 就是上一轮的会话号，agentproc 真的收到它
//!    （profile 把 `{{SESSION_ID}}` 回显进正文）；
//! 3. store 文件损坏时静默降级为冷会话，bridge 照常启动、不 panic。
//!
//! 观测方式沿用 `tests/wip_issue5_cli_session_store.rs`：`GatedTg` 包一层真实
//! `TelegramTransport`，从第 2 次 poll 起先等「上一轮回包写完了」，避免
//! dispatcher 紧跟的下一轮 poll 在持久化之前就把 store 读掉（真 flake）。
//!
//! Run: `cargo test --test wip_issue15_session_store_persistence`

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use im_agentproc::bridge::config::BridgeApp;
use im_agentproc::bridge::transport::{
    InboundOutcome, OutboundReply, SendOutcome, TelegramTransport, Transport, TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeStop};
use mockito::Matcher;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const TG_TOKEN: &str = "123:abc";

/// profile 子进程把 agentproc **实际收到的** `{{SESSION_ID}}` 回显进正文，
/// 结果事件固定声明 `session_id: "cli-1"`（于是第一轮之后磁盘上有号）。
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

/// 每次运行取一个全新 chat_id：store 现在会跨进程/跨运行存活，用例之间不能互相
/// 读到残留（本文件的用例都显式传 `tempfile` 路径，这里再加一层保险）。
fn fresh_chat_id(slot: i64) -> i64 {
    9_100_000_000 + (std::process::id() as i64 % 100_000) * 10 + slot
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

/// 收尾用的 401（`offset` = 下一轮），让 `run_bridge_with_shutdown` 以
/// `TokenRejected` 返回。
async fn mock_get_updates_401(server: &mut mockito::ServerGuard, offset: i64) -> mockito::Mock {
    server
        .mock("POST", "/bot123:abc/getUpdates")
        .match_body(Matcher::PartialJson(json!({ "offset": offset })))
        .with_status(401)
        .with_body(r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#)
        .create_async()
        .await
}

async fn mock_send_message(server: &mut mockito::ServerGuard) -> mockito::Mock {
    server
        .mock("POST", "/bot123:abc/sendMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .create_async()
        .await
}

fn transport_for(server: &mockito::ServerGuard, store_path: Option<PathBuf>) -> TelegramTransport {
    TelegramTransport::with_base_url_and_session_store(
        TG_TOKEN.to_string(),
        server.url(),
        store_path,
    )
    .expect("test transport")
}

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
            self.replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(reply.text.clone());
            let outcome = self.inner.send_reply(reply).await?;
            self.gate.notify_one();
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

async fn run_gated(transport: Arc<GatedTg>, yaml: &str) -> BridgeStop {
    let app = BridgeApp::parse_yaml(yaml, "issue15-store".to_string()).expect("profile yaml");
    tokio::time::timeout(
        Duration::from_secs(60),
        run_bridge_with_shutdown(transport, app, CancellationToken::new()),
    )
    .await
    .expect("bridge did not stop within 60s (gate deadlock?)")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telegram_resumes_after_bridge_restart_with_same_store_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("telegram.sessions.json");
    let chat_id = fresh_chat_id(1);

    // ── 运行 1（冷启动）：第 1 轮无 resume id，回包的 cli-1 落盘 ──────────────
    {
        let mut server = mockito::Server::new_async().await;
        let first = mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
        let send = mock_send_message(&mut server).await;
        let reject = mock_get_updates_401(&mut server, 2).await;

        let t = Arc::new(GatedTg::new(transport_for(
            &server,
            Some(store_path.clone()),
        )));
        assert_eq!(
            run_gated(t.clone(), PROFILE_YAML).await,
            BridgeStop::TokenRejected
        );

        first.assert_async().await;
        send.assert_async().await;
        reject.assert_async().await;
        assert_eq!(t.inbound_sessions(), vec![None], "第 1 轮必须是冷会话");
        assert_eq!(t.replies(), vec!["resume_input="]);
    }
    assert!(
        store_path.exists(),
        "bridge 运行后会话 store 必须落盘：{}",
        store_path.display()
    );

    // ── 运行 2（同一 store 路径 = bridge 重启）：本轮必须带上 cli-1 ────────────
    {
        let mut server = mockito::Server::new_async().await;
        let first = mock_get_updates(&mut server, 0, 1, chat_id, "again").await;
        let send = mock_send_message(&mut server).await;
        let reject = mock_get_updates_401(&mut server, 2).await;

        let t = Arc::new(GatedTg::new(transport_for(
            &server,
            Some(store_path.clone()),
        )));
        assert_eq!(
            run_gated(t.clone(), PROFILE_YAML).await,
            BridgeStop::TokenRejected
        );

        first.assert_async().await;
        send.assert_async().await;
        reject.assert_async().await;
        assert_eq!(
            t.inbound_sessions(),
            vec![Some("cli-1".to_string())],
            "重启后（同一 store 文件）下一轮必须带上上一轮的 cli_session_id"
        );
        assert_eq!(t.replies(), vec!["resume_input=cli-1"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_store_file_still_starts_bridge_and_runs_cold() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("telegram.sessions.json");
    std::fs::write(&store_path, b"not json{").expect("write corrupt store");
    let chat_id = fresh_chat_id(2);

    let mut server = mockito::Server::new_async().await;
    let first = mock_get_updates(&mut server, 0, 1, chat_id, "hi").await;
    let send = mock_send_message(&mut server).await;
    let reject = mock_get_updates_401(&mut server, 2).await;

    // 构造（= 加载损坏文件）不得 panic。
    let t = Arc::new(GatedTg::new(transport_for(&server, Some(store_path))));
    assert_eq!(
        run_gated(t.clone(), PROFILE_YAML).await,
        BridgeStop::TokenRejected
    );

    first.assert_async().await;
    send.assert_async().await;
    reject.assert_async().await;
    assert_eq!(
        t.inbound_sessions(),
        vec![None],
        "损坏的 store 必须降级为冷会话"
    );
    assert_eq!(t.replies(), vec!["resume_input="]);
}
