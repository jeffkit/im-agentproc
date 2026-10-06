//! 每轮会话上下文注入 —— issue #4：出站 MCP 的会话上下文必须是每轮的。
//!
//! 行为：每轮消息把**本轮** inbound 的 `context_token` 经
//! `RunOptions::extra_env` 注入 hub profile 子进程的
//! `IM_AGENTPROC_MCP_CONTEXT_TOKEN`，A 会话的 token 不会流进 B 会话的运行。
//!
//! 这里刻意先在 bridge 进程 env 里塞一个运维一次性 export 的固定 token，
//! 再用两个不同会话的并发 turn 验证它被本轮值覆盖。
//!
//! 运行：`cargo test --test mcp_per_turn_context`。

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

/// 运维在 bridge 进程里一次性 export 的 token（今天 docs 教的部署方式）。
const STALE_PROCESS_TOKEN: &str = "operator-exported-once";

/// profile 子进程把 `IM_AGENTPROC_MCP_CONTEXT_TOKEN` 原样回显成 agentproc
/// 的 `result` 事件，于是回复正文即该轮子进程实际看到的 token。
const PROFILE_YAML: &str = r#"
agentproc:
  command: /bin/sh
  args:
    - -c
    - |
      printf '{"type":"result","text":"%s"}\n' "$IM_AGENTPROC_MCP_CONTEXT_TOKEN"
  timeout_secs: 20
  streaming: false
"#;

/// 第一轮 poll 投递 ctx-a / ctx-b 两条消息，之后返回 TokenRejected 让
/// run_bridge_with_shutdown 收尾；send_reply 记录 (context_token, text)。
struct FakeIm {
    polls: AtomicUsize,
    sends: Mutex<Vec<(String, String)>>,
}

impl FakeIm {
    fn new() -> Self {
        Self {
            polls: AtomicUsize::new(0),
            sends: Mutex::new(Vec::new()),
        }
    }

    fn sent(&self) -> Vec<(String, String)> {
        self.sends.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn wait_for_sends(&self, n: usize, timeout: Duration) -> Vec<(String, String)> {
        let deadline = Instant::now() + timeout;
        loop {
            let sent = self.sent();
            if sent.len() >= n || Instant::now() >= deadline {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Transport for FakeIm {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            if self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(InboundOutcome::Messages(vec![
                    inbound("ctx-a"),
                    inbound("ctx-b"),
                ]))
            } else {
                Ok(InboundOutcome::TokenRejected)
            }
        })
    }

    fn send_reply<'a>(
        &'a self,
        reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
        Box::pin(async move {
            self.sends
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((reply.context_token, reply.text));
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            media_upload: true,
            max_text_len: None,
        }
    }
}

fn inbound(ctx: &str) -> InboundMessage {
    InboundMessage {
        context_token: Some(ctx.to_string()),
        from_user: Some(format!("user-of-{ctx}")),
        is_from_bot: false,
        text: Some("hello".to_string()),
        media: vec![],
        session_id: Some(String::new()),
        session_name: Some("default".to_string()),
        dispatch_key: None,
        a2a_call_id: None,
        extra: serde_json::Value::Null,
        raw: serde_json::Value::Null,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_turn_injects_its_own_context_token() {
    // 模拟文档里的部署：运维为整个 bridge 进程 export 了一个固定 token。
    // SAFETY: 本集成测试二进制只有这一个测试，且是唯一读写这些变量的地方。
    unsafe {
        std::env::set_var("IM_AGENTPROC_MCP_CONTEXT_TOKEN", STALE_PROCESS_TOKEN);
        std::env::set_var("IM_AGENTPROC_MCP_TRANSPORT", "telegram");
    }

    let app = BridgeApp::parse_yaml(PROFILE_YAML, "mcp-ctx".to_string()).expect("profile yaml");
    let im = Arc::new(FakeIm::new());
    let stop = run_bridge_with_shutdown(im.clone(), app, CancellationToken::new()).await;
    assert_eq!(
        stop,
        BridgeStop::TokenRejected,
        "bridge should stop once the fake transport rejects the token"
    );

    let sends = im.wait_for_sends(2, Duration::from_secs(20)).await;

    unsafe {
        std::env::remove_var("IM_AGENTPROC_MCP_CONTEXT_TOKEN");
        std::env::remove_var("IM_AGENTPROC_MCP_TRANSPORT");
    }

    assert_eq!(
        sends.len(),
        2,
        "each of the two turns must produce one final reply, got {sends:?}"
    );
    let text_for = |ctx: &str| {
        sends
            .iter()
            .find(|(c, _)| c == ctx)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| panic!("no reply captured for {ctx}; got {sends:?}"))
    };
    let a = text_for("ctx-a");
    let b = text_for("ctx-b");

    assert_ne!(
        a, b,
        "两个不同会话的 turn 共用了同一个 IM_AGENTPROC_MCP_CONTEXT_TOKEN \
         (两个 profile 子进程都看到 {a:?} => 跨会话串台)"
    );
    assert_eq!(
        a, "ctx-a",
        "turn A 的 profile 子进程看到 {a:?}，不是本轮 inbound 的 context_token"
    );
    assert_eq!(
        b, "ctx-b",
        "turn B 的 profile 子进程看到 {b:?}，不是本轮 inbound 的 context_token"
    );
}
