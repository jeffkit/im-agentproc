//! End-to-end tests for issue #6 — delivery semantics: a message is durably
//! recorded before dispatch, redelivered after a crash, and never answered
//! twice; the Telegram `getUpdates` offset survives a restart and filters
//! re-delivered updates.
//!
//! (Originally landed as `tests/[wip]_issue6_delivery.rs`; the brackets made the
//! file an invalid cargo test target name, so it was renamed to build.)
//!
//! Acceptance clauses covered here (integration level, public API only):
//!   1. inbound message is durably recorded BEFORE dispatch, and is redelivered
//!      after a hard kill that happens after dispatch but before the reply —
//!      and NOT replayed once the reply succeeded.
//!   2. Telegram `getUpdates` offset is persisted and resumed by a rebuilt
//!      transport (process restart) instead of resolving from an empty in-memory
//!      cursor; a re-delivered `update_id` is de-duplicated.
//!
//! In-crate unit coverage (needs `pub(crate)`/`pub(super)` seams) lives in
//! `src/bridge/dispatcher/tests.rs` (`wip_*`), `src/bridge/dispatcher/wal.rs`
//! and the Telegram transport test module.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use im_agentproc::bridge::transport::{
    InboundMessage, InboundOutcome, OutboundReply, SendOutcome, TelegramTransport, Transport,
    TransportCapabilities,
};
use im_agentproc::bridge::{run_bridge_with_shutdown, BridgeApp};
use mockito::{Matcher, Server};
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

// ── helpers ────────────────────────────────────────────────────────────────

/// `HOME` is process-global; the tests that repoint it must not run
/// concurrently (each of them runs a bridge whose state dir resolves from it).
fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Current-thread runtime + `block_on`, so a `MutexGuard` held by `#[test]`
/// bodies never lives across an `.await` (keeps `clippy::await_holding_lock`
/// quiet under `-D warnings`).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime")
        .block_on(f)
}

/// Recursively collect files under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(body) = std::fs::read_to_string(&path) {
                if body.contains(needle) {
                    hits.push(path);
                }
            }
        }
    }
    hits
}

/// Write a minimal agentproc 0.4 handler that answers every turn with a
/// fixed `result` event, so a dispatched message deterministically produces a
/// non-empty reply body.
fn write_handler(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("handler.sh");
    std::fs::write(
        &path,
        format!(
            "#!/usr/bin/env bash\ncat >/dev/null\nprintf '%s\\n' '{{\"type\":\"result\",\"text\":\"{text}\"}}'\n"
        ),
    )
    .expect("write handler");
    path
}

fn app_for(handler: &Path) -> BridgeApp {
    BridgeApp::parse_yaml(
        &format!(
            "script: {}\nagentproc:\n  timeout_secs: 20\n  streaming: false\n",
            handler.display()
        ),
        "wip-issue6".to_string(),
    )
    .expect("parse bridge profile")
}

fn inbound(ctx: &str, text: &str) -> InboundMessage {
    InboundMessage {
        context_token: Some(ctx.to_string()),
        from_user: Some("user-1".to_string()),
        is_from_bot: false,
        text: Some(text.to_string()),
        media: vec![],
        session_id: None,
        session_name: Some("default".to_string()),
        a2a_call_id: None,
        extra: serde_json::Value::Null,
        raw: serde_json::Value::Null,
    }
}

/// Scripted transport: hands out its queued inbound messages once, records
/// every reply, and (optionally) parks its very first `send_reply` until
/// [`MockTransport::release`] is notified — that parked call is the
/// "dispatched but not yet replied" crash point.
#[derive(Clone)]
struct MockTransport {
    inbound: Arc<Mutex<Vec<InboundMessage>>>,
    replies: Arc<Mutex<Vec<OutboundReply>>>,
    send_attempts: Arc<AtomicUsize>,
    block_first_send: bool,
    release: Arc<Notify>,
}

impl MockTransport {
    fn new(msgs: Vec<InboundMessage>, block_first_send: bool) -> Self {
        Self {
            inbound: Arc::new(Mutex::new(msgs)),
            replies: Arc::new(Mutex::new(Vec::new())),
            send_attempts: Arc::new(AtomicUsize::new(0)),
            block_first_send,
            release: Arc::new(Notify::new()),
        }
    }

    fn replies(&self) -> Vec<OutboundReply> {
        self.replies.lock().expect("replies poisoned").clone()
    }

    fn send_attempts(&self) -> usize {
        self.send_attempts.load(Ordering::SeqCst)
    }

    fn reply_contexts(&self) -> Vec<String> {
        self.replies()
            .into_iter()
            .map(|r| r.context_token)
            .collect()
    }
}

impl Transport for MockTransport {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        Box::pin(async move {
            let drained: Vec<InboundMessage> = {
                let mut queue = self.inbound.lock().expect("inbound poisoned");
                std::mem::take(&mut *queue)
            };
            if drained.is_empty() {
                // Avoid a hot poll loop when there is nothing to deliver.
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(InboundOutcome::Messages(drained))
        })
    }

    fn send_reply<'a>(
        &'a self,
        reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
        let attempt = self.send_attempts.fetch_add(1, Ordering::SeqCst);
        self.replies.lock().expect("replies poisoned").push(reply);
        let block = self.block_first_send && attempt == 0;
        let release = self.release.clone();
        Box::pin(async move {
            if block {
                release.notified().await;
            }
            Ok(SendOutcome::Sent)
        })
    }

    fn name(&self) -> &'static str {
        "wip-mock"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

async fn wait_until(label: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {label}");
}

// ── control: the harness itself ───────────────────────────────────────────

/// Sanity check for the harness: one inbound message, no crash, exactly one
/// reply carrying the inbound context token. Passes before and after the fix
/// (it exists so a failure of the `wip_*` tests can be attributed to the bug
/// rather than to a broken fixture).
#[test]
fn control_single_message_produces_exactly_one_reply() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().expect("tempdir");
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let result = block_on(async {
        let work = tempfile::tempdir().expect("tempdir");
        let handler = write_handler(work.path(), "ok");
        let app = app_for(&handler);
        let transport = MockTransport::new(vec![inbound("ctx-control", "ping")], false);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_bridge_with_shutdown(
            Arc::new(transport.clone()),
            app,
            shutdown.clone(),
        ));

        wait_until("one reply", Duration::from_secs(20), || {
            transport.send_attempts() >= 1
        })
        .await;

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        transport.reply_contexts()
    });

    match previous_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }

    assert_eq!(
        result,
        vec!["ctx-control".to_string()],
        "control: exactly one reply for one inbound message"
    );
}

// ── acceptance 1: WAL + crash redelivery ──────────────────────────────────

/// The crash point of issue #6: the message was taken from the transport and
/// dispatched, the reply had not landed yet, the process is killed.
///
/// Required behaviour:
///   * the message is on disk BEFORE dispatch (so a kill cannot lose it),
///   * a restarted bridge redelivers it and answers it,
///   * once answered, a further restart does NOT replay it again
///     (at-least-once delivery with completion marking).
#[test]
fn wip_crashed_message_is_redelivered_after_restart_and_not_replayed_once_answered() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().expect("tempdir");
    let wal_env = tempfile::tempdir().expect("tempdir");
    let previous_home = std::env::var_os("HOME");
    let previous_wal = std::env::var_os("IM_AGENTPROC_WAL_DIR");
    std::env::set_var("HOME", home.path());
    // The fix may resolve its WAL directory from either the data dir (HOME) or
    // this override; point both at a scratch dir so the test never touches the
    // developer's real state.
    std::env::set_var("IM_AGENTPROC_WAL_DIR", wal_env.path());

    let outcome = block_on(async {
        let work = tempfile::tempdir().expect("tempdir");
        let handler = write_handler(work.path(), "ok");
        let app = app_for(&handler);

        // ── run 1: dispatch, then die before the reply lands ──────────────
        let run1 = MockTransport::new(vec![inbound("ctx-wal", "wal-hello")], true);
        let shutdown1 = CancellationToken::new();
        let bridge1 = tokio::spawn(run_bridge_with_shutdown(
            Arc::new(run1.clone()),
            app.clone(),
            shutdown1.clone(),
        ));
        wait_until(
            "reply attempt #1 (message dispatched, send parked)",
            Duration::from_secs(20),
            || run1.send_attempts() >= 1,
        )
        .await;
        let mut wal_hits = files_containing(home.path(), "ctx-wal");
        wal_hits.extend(files_containing(wal_env.path(), "ctx-wal"));
        // Hard kill: abort the task (the parked `send_reply` never returns).
        bridge1.abort();
        let _ = bridge1.await;

        // ── run 2: restart, the pending message must be re-dispatched ─────
        let run2 = MockTransport::new(vec![], false);
        let shutdown2 = CancellationToken::new();
        let bridge2 = tokio::spawn(run_bridge_with_shutdown(
            Arc::new(run2.clone()),
            app.clone(),
            shutdown2.clone(),
        ));
        let redelivered = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if run2.reply_contexts().contains(&"ctx-wal".to_string()) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        shutdown2.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), bridge2).await;

        // ── run 3: the completed entry must not replay ────────────────────
        let run3 = MockTransport::new(vec![], false);
        let shutdown3 = CancellationToken::new();
        let bridge3 = tokio::spawn(run_bridge_with_shutdown(
            Arc::new(run3.clone()),
            app.clone(),
            shutdown3.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let replayed = run3.send_attempts();
        shutdown3.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), bridge3).await;

        (wal_hits, redelivered, replayed)
    });

    match previous_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match previous_wal {
        Some(v) => std::env::set_var("IM_AGENTPROC_WAL_DIR", v),
        None => std::env::remove_var("IM_AGENTPROC_WAL_DIR"),
    }

    let (wal_hits, redelivered, replayed) = outcome;
    assert!(
        !wal_hits.is_empty(),
        "issue #6 acceptance 1: the inbound message must be durably written \
         (WAL/log) BEFORE dispatch, so a kill between dispatch and reply cannot lose it; \
         no file under $HOME contains the pending context token `ctx-wal`"
    );
    assert!(
        redelivered,
        "issue #6 acceptance 1: a message killed after dispatch and before the reply \
         must be redelivered after restart; run 2 never answered `ctx-wal`"
    );
    assert_eq!(
        replayed, 0,
        "issue #6 acceptance 1: a WAL entry whose reply succeeded must be deleted/marked \
         done — run 3 replayed a completed message"
    );
}

// ── acceptance 4: Telegram offset persistence + dedup ─────────────────────

const TG_TOKEN: &str = "123456:WIPISSUE6";

fn tg_updates_body(update_id: i64, text: &str) -> String {
    json!({
        "ok": true,
        "result": [{
            "update_id": update_id,
            "message": {
                "message_id": update_id,
                "date": 0,
                "chat": { "id": 42, "type": "private" },
                "from": { "id": 7, "first_name": "u", "is_bot": false },
                "text": text
            }
        }]
    })
    .to_string()
}

/// The `getUpdates` offset must survive a process restart: a freshly built
/// transport (the `run_loop` always starts with an empty in-memory `buf`) must
/// resume from the persisted offset instead of polling `offset: 0` again.
#[test]
fn wip_telegram_offset_is_persisted_and_resumed_by_a_rebuilt_transport() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().expect("tempdir");
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let outcome = block_on(async {
        let mut server = Server::new_async().await;
        let get_updates = format!("/bot{TG_TOKEN}/getUpdates");

        let offset_zero = server
            .mock("POST", get_updates.as_str())
            .match_body(Matcher::PartialJson(json!({ "offset": 0 })))
            .with_status(200)
            .with_body(tg_updates_body(42, "hello"))
            .expect(1)
            .create_async()
            .await;
        let offset_resumed = server
            .mock("POST", get_updates.as_str())
            .match_body(Matcher::PartialJson(json!({ "offset": 43 })))
            .with_status(200)
            .with_body(r#"{"ok":true,"result":[]}"#)
            .expect(1)
            .create_async()
            .await;

        // Process #1: one poll, which must record offset 43 (update 42 + 1).
        let first = TelegramTransport::with_base_url(TG_TOKEN.to_string(), server.url())
            .expect("transport");
        let mut buf = String::new();
        let first_outcome = first.next_inbound(&mut buf).await;
        let first_delivered = matches!(
            first_outcome,
            Ok(InboundOutcome::Messages(ref m)) if m.len() == 1
        );
        drop(first);

        // Process #2 (restart): brand-new transport, brand-new empty cursor.
        let rebuilt = TelegramTransport::with_base_url(TG_TOKEN.to_string(), server.url())
            .expect("transport");
        let mut fresh_buf = String::new();
        let resumed = rebuilt.next_inbound(&mut fresh_buf).await.is_ok();

        offset_zero.assert_async().await;
        let resumed_called = offset_resumed.expect(1).matched_async().await;
        (first_delivered, resumed, resumed_called, fresh_buf.clone())
    });

    match previous_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }

    let (first_delivered, resumed, resumed_called, fresh_buf) = outcome;
    assert!(first_delivered, "poll #1 must deliver update_id 42");
    assert!(
        resumed,
        "issue #6 acceptance 4: the restart poll must succeed by resuming the \
         persisted offset (43); it failed"
    );
    assert!(
        resumed_called,
        "issue #6 acceptance 4: after a restart the transport must poll getUpdates with \
         the persisted offset 43 (it must not resolve the offset from the in-memory buf, which \
         is empty on restart); the offset=43 request was never made (fresh buf = {fresh_buf:?})"
    );
}

/// Telegram re-delivers updates that were not confirmed (or the bot is
/// restarted mid-batch). An `update_id` / `message_id` already seen must not be
/// handed to the dispatcher twice — the duplicate would re-run an expensive CLI
/// turn for one user message.
#[test]
fn wip_telegram_duplicate_update_id_is_dropped_on_repoll() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().expect("tempdir");
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let outcome = block_on(async {
        let mut server = Server::new_async().await;
        // The server always answers with the SAME update_id, whatever offset it
        // is asked for (i.e. it ignores the confirmation) — the transport must
        // de-duplicate on its own.
        let _same_update = server
            .mock("POST", format!("/bot{TG_TOKEN}/getUpdates").as_str())
            .with_status(200)
            .with_body(tg_updates_body(42, "hello"))
            .expect(2)
            .create_async()
            .await;

        let transport = TelegramTransport::with_base_url(TG_TOKEN.to_string(), server.url())
            .expect("transport");
        let mut buf = String::new();
        let first = transport.next_inbound(&mut buf).await;
        let second = transport.next_inbound(&mut buf).await;
        let count = |outcome: &anyhow::Result<InboundOutcome>| match outcome {
            Ok(InboundOutcome::Messages(msgs)) => msgs.len(),
            _ => 0,
        };
        (count(&first), count(&second))
    });

    match previous_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }

    let (first_count, second_count) = outcome;
    assert_eq!(first_count, 1, "poll #1 must deliver the fresh update");
    assert_eq!(
        second_count, 0,
        "issue #6 acceptance 4: a re-delivered update_id (42) must be de-duplicated — the \
         dispatcher must not run the CLI twice for one user message"
    );
}
