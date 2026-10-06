//! issue #15 子项 2 的定向回归：wecom 同一 chatid 的并发入站按到达顺序串行处理。
//!
//! wecom 的 `context_token` 是**每条消息一个**的 `req_id`（回复必须逐字回填），
//! 而 dispatcher 的会话 worker 以 `context_token:session_name` 为键 ⇒ 同一 chat
//! 连发两条会各起一个 worker、并发跑 CLI（会话号互相覆盖、回复顺序与入站顺序不一致）。
//! 修复形态是通用 DTO 上的 `dispatch_key`（IM 无关）：wecom 填 `wecom:{chatid}`，
//! 其余通道留 `None`；dispatcher 只认这个键，不认识任何 IM。
//!
//! 本用例走公开 API（`run_bridge_with_shutdown` + 自造 `Transport`）：
//! 一次 poll 投递两条 wecom 形状的消息（同 `dispatch_key`、不同 `context_token`），
//! profile 每轮往同一个 trace 文件追加 `start` / `sleep 1` / `end`。
//! 串行 ⇒ `["start","end","start","end"]`；并发 ⇒ 交错（`start,start,end,end`）。
//!
//! Run: `cargo test --test wip_issue15_wecom_serialization`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use im_agentproc::bridge::config::BridgeApp;
use im_agentproc::bridge::transport::{
    InboundMessage, InboundOutcome, OutboundReply, SendOutcome, Transport, TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeStop};
use tokio_util::sync::CancellationToken;

/// 每轮各写 start / end（中间 sleep 1s），进程内并发时两次 `sleep` 会重叠。
fn trace_profile_yaml(trace_path: &std::path::Path) -> String {
    format!(
        r#"
agentproc:
  command: /bin/sh
  args:
    - -c
    - |
      printf 'start\n' >> {trace}
      sleep 1
      printf 'end\n' >> {trace}
      printf '{{"type":"result","text":"done"}}\n'
  timeout_secs: 30
  streaming: false
"#,
        trace = trace_path.display()
    )
}

/// 同一 chat 的两条消息：`context_token`（= wecom 的 `req_id`）不同，
/// `dispatch_key` 相同。
fn wecom_like_inbound(req_id: &str) -> InboundMessage {
    InboundMessage {
        context_token: Some(req_id.to_string()),
        from_user: Some("u_1".to_string()),
        is_from_bot: false,
        text: Some("hi".to_string()),
        media: vec![],
        session_id: None,
        session_name: Some("wecom-single".to_string()),
        dispatch_key: Some("wecom:chat-1".to_string()),
        a2a_call_id: None,
        extra: serde_json::Value::Null,
        raw: serde_json::Value::Null,
    }
}

/// 第 1 次 poll 投递两条消息；之后等两个回包都到齐，再返回 `TokenRejected` 收尾。
struct TwoMsgTransport {
    pending: Mutex<Vec<InboundMessage>>,
    replies: Mutex<Vec<OutboundReply>>,
    polls: AtomicUsize,
}

impl TwoMsgTransport {
    fn new(msgs: Vec<InboundMessage>) -> Self {
        Self {
            pending: Mutex::new(msgs),
            replies: Mutex::new(Vec::new()),
            polls: AtomicUsize::new(0),
        }
    }

    fn reply_count(&self) -> usize {
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn replies(&self) -> Vec<OutboundReply> {
        self.replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Transport for TwoMsgTransport {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            if self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                let batch: Vec<InboundMessage> =
                    std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
                return Ok(InboundOutcome::Messages(batch));
            }
            // 两个回包都送达后才收尾，保证 trace 文件已经写完。
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.reply_count() < 2 {
                if Instant::now() > deadline {
                    panic!("30s 内没有等到两个回包");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(InboundOutcome::TokenRejected)
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
                .push(reply);
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "wip-issue15-wecom"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_chat_messages_are_handled_in_order_not_concurrently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let trace = dir.path().join("trace.txt");
    let yaml = trace_profile_yaml(&trace);
    let app = BridgeApp::parse_yaml(&yaml, "issue15-wecom".to_string()).expect("profile yaml");

    let transport = Arc::new(TwoMsgTransport::new(vec![
        wecom_like_inbound("req-1"),
        wecom_like_inbound("req-2"),
    ]));
    let stop = tokio::time::timeout(
        Duration::from_secs(60),
        run_bridge_with_shutdown(transport.clone(), app, CancellationToken::new()),
    )
    .await
    .expect("bridge did not stop within 60s");
    assert_eq!(stop, BridgeStop::TokenRejected);

    let replies = transport.replies();
    assert_eq!(replies.len(), 2, "两条消息都必须有回包：{replies:?}");
    assert!(replies.iter().all(|r| r.text.contains("done")));

    let trace_body = std::fs::read_to_string(&trace).expect("trace file");
    let lines: Vec<&str> = trace_body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines,
        vec!["start", "end", "start", "end"],
        "同一 chat 的两轮必须串行（并发时两轮 sleep 重叠，trace 会交错）：{trace_body:?}"
    );
}
