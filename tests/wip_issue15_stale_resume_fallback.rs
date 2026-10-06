//! issue #15 子项 3 的定向回归：resume 失效（会话不存在）自动回退冷启动重跑。
//!
//! 链路：`InboundMessage.session_id` → `RunOptions.session_id` → executor 拼
//! `--resume <id>`。CLI 说该会话不存在时（Claude Code 实测措辞
//! `No conversation found with session ID: <uuid>`），#5 之前的实现把这一轮直接
//! 变成「（本地 CLI 失败）」错误回复、消息内容丢失。修复后：识别为 stale resume，
//! 用**空 session** 重跑同一轮；非 stale 的错误仍然照原样报错。
//!
//! profile 走 `sh -c` + `{{SESSION_ID}}`（`{{MESSAGE}}` 会在加载期被拒，
//! 见 `src/bridge/config.rs`），第 1 个位置参数落在 `$0`；`-n "$0"` 因此就是
//! 「这一轮带没带 resume id」的判据。
//!
//! Run: `cargo test --test wip_issue15_stale_resume_fallback`

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use im_agentproc::bridge::config::BridgeApp;
use im_agentproc::bridge::transport::{
    InboundMessage, InboundOutcome, OutboundReply, SendOutcome, Transport, TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeStop};
use tokio_util::sync::CancellationToken;

/// 带 resume id 时 CLI 报「会话不存在」，否则冷启动成功。
fn stale_then_cold_yaml(error_message: &str) -> String {
    format!(
        r#"
agentproc:
  command: /bin/sh
  args:
    - -c
    - |
      if [ -n "$0" ]; then
        printf '{{"type":"error","message":"{error_message}: %s"}}\n' "$0"
        exit 1
      fi
      printf '{{"type":"result","text":"cold-ok","session_id":"cli-2"}}\n'
    - "{{{{SESSION_ID}}}}"
  send_error_reply: true
  timeout_secs: 10
  streaming: false
"#
    )
}

fn inbound(ctx: &str, session_id: Option<&str>) -> InboundMessage {
    InboundMessage {
        context_token: Some(ctx.to_string()),
        from_user: Some("user-1".to_string()),
        is_from_bot: false,
        text: Some("hello".to_string()),
        media: vec![],
        session_id: session_id.map(str::to_string),
        session_name: Some("default".to_string()),
        dispatch_key: None,
        a2a_call_id: None,
        extra: serde_json::Value::Null,
        raw: serde_json::Value::Null,
    }
}

/// 投递一条消息，之后继续返回空批次；用例等到回包后再取消 shutdown。
struct OneShotTransport {
    pending: Mutex<Vec<InboundMessage>>,
    replies: Mutex<Vec<OutboundReply>>,
}

impl OneShotTransport {
    fn new(msg: InboundMessage) -> Self {
        Self {
            pending: Mutex::new(vec![msg]),
            replies: Mutex::new(Vec::new()),
        }
    }

    fn replies(&self) -> Vec<OutboundReply> {
        self.replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Transport for OneShotTransport {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            let next = self.pending.lock().unwrap_or_else(|e| e.into_inner()).pop();
            match next {
                Some(msg) => Ok(InboundOutcome::Messages(vec![msg])),
                None => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(InboundOutcome::Messages(Vec::new()))
                }
            }
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
        "wip-issue15-stale-resume"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

/// 跑一轮 bridge，等到有回包（或超时）后收尾；返回用户实际收到的回包。
async fn replies_for(yaml: &str, msg: InboundMessage) -> Vec<OutboundReply> {
    let transport = Arc::new(OneShotTransport::new(msg));
    let app = BridgeApp::parse_yaml(yaml, "issue15-stale".to_string()).expect("profile yaml");
    let shutdown = CancellationToken::new();
    let mut run = Box::pin(run_bridge_with_shutdown(
        transport.clone(),
        app,
        shutdown.clone(),
    ));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !transport.replies().is_empty() {
            break;
        }
        if Instant::now() > deadline {
            panic!("30s 内没有收到任何回包");
        }
        if let Ok(stop) = tokio::time::timeout(Duration::from_millis(50), &mut run).await {
            panic!("bridge 在回包前自己停了：{stop:?}");
        }
    }

    shutdown.cancel();
    let stop = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("bridge 未在 shutdown 后退出");
    assert_eq!(stop, BridgeStop::Shutdown);
    transport.replies()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_session_id_falls_back_to_cold_run() {
    let yaml = stale_then_cold_yaml("No conversation found with session ID");
    let replies = replies_for(&yaml, inbound("ctx-stale", Some("stale-1"))).await;

    assert_eq!(
        replies.len(),
        1,
        "回退重跑只能产生一个用户可见回包：{replies:?}"
    );
    let text = &replies[0].text;
    assert!(
        text.contains("cold-ok"),
        "stale resume 必须回退冷启动重跑（用户应收到 cold-ok），实际收到 {text:?}"
    );
    assert!(
        !text.contains("（本地 CLI 失败）"),
        "stale resume 不该再报错，实际收到 {text:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_stale_error_still_reports_failure() {
    let yaml = stale_then_cold_yaml("upstream 500 internal error");
    let replies = replies_for(&yaml, inbound("ctx-other", Some("stale-1"))).await;

    assert_eq!(replies.len(), 1, "回包数：{replies:?}");
    let text = &replies[0].text;
    assert!(
        text.contains("（本地 CLI 失败）"),
        "非 stale 的 CLI 错误必须照原样报错，实际收到 {text:?}"
    );
    assert!(
        !text.contains("cold-ok"),
        "非 stale 错误不得触发冷启动重跑，实际收到 {text:?}"
    );
}
