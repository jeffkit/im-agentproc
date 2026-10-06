//! `[wip]` repro tests for issue #3 — a single message whose CLI error text
//! merely *contains* an over-broad keyword (`"token"`) or the substring
//! `"not found"` / `"no such file"` is classified as an auth failure
//! (`HandleError::Fatal(BridgeStop::FatalCliError)`), which makes
//! `run_bridge_reconnecting` bail and kill the whole bridge process. Every
//! ordinary LLM-side error (`max_tokens`, `tokens exceeded`,
//! `model not found`, `tool not found`, `404 not found`) therefore restarts
//! the bridge for ALL tenants — a single-user DoS.
//!
//! These are *investigation* tests: they encode the acceptance criteria and
//! MUST FAIL against the current tree. They must keep passing once the fix
//! lands. Everything here uses the public API only (`run_bridge_with_shutdown`
//! + the `Transport` trait), so no `pub(crate)` seam is needed.
//!
//! Observation mechanism: the fake transport hands out one inbound message
//! (whose profile CLI exits non-zero with a given stderr line) and then keeps
//! returning empty batches. A `FatalCliError` classification reaches
//! `run_bridge_with_shutdown`'s `stop_rx` and the function returns within the
//! race window; a `Transient` classification keeps the bridge polling until
//! the shutdown token is cancelled (returns `BridgeStop::Shutdown`).
//!
//!   * `control_*`                — expectation holds before AND after the fix
//!     (fixture self-check / anti-regression anchors).
//!   * `wip_*_is_transient*`      — FAIL today (bridge stops with FatalCliError).
//!   * `*_still_fatal`            — genuine auth failures must STAY fatal.
//!
//! Run: `cargo test --test wip_issue3_fatal_cli_classification`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use im_agentproc::bridge::transport::{
    InboundMessage, InboundOutcome, OutboundReply, SendOutcome, Transport, TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeApp, BridgeStop};
use tokio_util::sync::CancellationToken;

/// How long we wait for the bridge to stop *by itself* after the failing
/// message was dispatched. The CLI exits immediately, so a FatalCliError
/// signal lands well inside this window.
const SELF_STOP_WINDOW: Duration = Duration::from_millis(1500);

enum Observation {
    /// The bridge returned on its own before the window elapsed.
    StoppedEarly(BridgeStop),
    /// The bridge was still polling; the returned stop reason follows shutdown.
    StillPolling(BridgeStop),
}

/// A `sh -c` profile that exits 1 after writing `stderr_line` to stderr is
/// enough: agentproc turns that into `RunResult.error = "agent exited with
/// code 1: <stderr>"`, which `run_via_agentproc` turns into the `anyhow`
/// error text that `handle_one_message` classifies.
fn app_yaml(stderr_line: &str) -> String {
    format!(
        "agentproc:\n  command: sh\n  args: [\"-c\", \"echo '{stderr_line}' >&2; exit 1\"]\n  timeout_secs: 10\n  streaming: false\n"
    )
}

fn inbound(ctx: &str) -> InboundMessage {
    InboundMessage {
        context_token: Some(ctx.to_string()),
        from_user: Some("user-1".to_string()),
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

/// Guard lives inside this fn, so the fake transport never holds a std lock
/// across an `.await` (keeps `clippy::await_holding_lock` quiet).
fn take_pending(pending: &Mutex<Vec<InboundMessage>>) -> Option<InboundMessage> {
    pending.lock().expect("pending lock").pop()
}

/// Hands out one message, then keeps the long-poll loop alive with empty
/// batches so a `stop_rx` signal (fatal) can be observed.
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
}

impl Transport for OneShotTransport {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            match take_pending(&self.pending) {
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
            self.replies.lock().expect("replies lock").push(reply);
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "wip-issue3"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

async fn observe(yaml: &str) -> (Observation, Arc<OneShotTransport>) {
    let transport = Arc::new(OneShotTransport::new(inbound("ctx-wip-issue3")));
    let app = BridgeApp::parse_yaml(yaml, "wip-issue3".to_string()).expect("parse bridge profile");
    let shutdown = CancellationToken::new();
    let mut run = Box::pin(run_bridge_with_shutdown(
        transport.clone(),
        app,
        shutdown.clone(),
    ));

    match tokio::time::timeout(SELF_STOP_WINDOW, &mut run).await {
        Ok(stop) => (Observation::StoppedEarly(stop), transport),
        Err(_) => {
            shutdown.cancel();
            let stop = run.await;
            (Observation::StillPolling(stop), transport)
        }
    }
}

/// The error text must be treated as a per-message transient failure: the
/// bridge keeps serving everyone else (and exits cleanly on shutdown).
async fn assert_transient(stderr_line: &str) {
    let (observed, transport) = observe(&app_yaml(stderr_line)).await;
    match observed {
        Observation::StoppedEarly(BridgeStop::FatalCliError(reason)) => panic!(
            "error text {stderr_line:?} must NOT be classified as a fatal auth error; \
             bridge stopped the whole process with FatalCliError: {reason}"
        ),
        Observation::StoppedEarly(other) => {
            panic!("error text {stderr_line:?}: unexpected early stop {other:?}")
        }
        Observation::StillPolling(stop) => {
            assert_eq!(
                stop,
                BridgeStop::Shutdown,
                "error text {stderr_line:?}: bridge must keep polling until shutdown"
            );
        }
    }
    let replies = transport.replies.lock().expect("replies lock").len();
    assert!(
        replies >= 1,
        "error text {stderr_line:?}: the user must still get the CLI-error reply"
    );
}

/// A genuine credential failure must still stop the bridge, unchanged.
async fn assert_still_fatal(stderr_line: &str) {
    let (observed, _transport) = observe(&app_yaml(stderr_line)).await;
    match observed {
        Observation::StoppedEarly(BridgeStop::FatalCliError(_)) => {}
        Observation::StoppedEarly(other) => {
            panic!("genuine auth failure {stderr_line:?} must be Fatal, got {other:?}")
        }
        Observation::StillPolling(stop) => panic!(
            "genuine auth failure {stderr_line:?} must stop the bridge as FatalCliError, \
             but it kept polling (stop = {stop:?})"
        ),
    }
}

// ── controls: must hold before and after the fix ──────────────────────────

/// Fixture self-check: a failure with no keyword at all is already transient
/// today, proving the harness can observe both outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_non_keyword_failure_is_transient() {
    assert_transient("rate limit hit, please retry later").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_invalid_api_key_still_fatal() {
    assert_still_fatal("invalid api key provided").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_not_logged_in_still_fatal() {
    assert_still_fatal("not logged in: run `login` first").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_401_unauthorized_still_fatal() {
    assert_still_fatal("401 unauthorized").await;
}

// ── the bug: ordinary LLM-side errors must not kill the process ───────────

/// `"token"` in `AUTH_ERROR_KEYWORDS` matches `max_tokens`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wip_max_tokens_error_is_transient() {
    assert_transient("API error: max_tokens(4096) reached").await;
}

/// Acceptance wording `"tokens exceeded"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wip_tokens_exceeded_error_is_transient() {
    assert_transient("context tokens exceeded (200001 > 200000)").await;
}

/// Matches the `contains("not found")` arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wip_model_not_found_error_is_transient() {
    assert_transient("model not found: claude-nonexistent-9").await;
}

/// Matches the `contains("not found")` arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wip_tool_not_found_error_is_transient() {
    assert_transient("tool not found: mcp__demo__missing").await;
}

/// Matches the `contains("not found")` arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wip_404_not_found_error_is_transient() {
    assert_transient("upstream responded 404 not found").await;
}
