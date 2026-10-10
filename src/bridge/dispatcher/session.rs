use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bridge::config::BridgeApp;
use crate::bridge::transport::{InboundMessage, OutboundReply, SendOutcome, Transport};

use super::handle::handle_one_message;
use super::send::sanitize_errmsg;
use super::wal::Wal;
use super::BridgeStop;

pub(super) fn session_dispatch_key(msg: &InboundMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg.session_name.as_deref().unwrap_or("default");
    format!("{ctx}:{session_name}")
}

/// One dispatched message plus the WAL entry that must be dropped once its
/// reply has landed. `wal_id` is `None` only when the message was never
/// recorded (no WAL attached, or the write failed).
struct Queued {
    msg: InboundMessage,
    wal_id: Option<String>,
}

/// The in-memory queue in front of one session worker, plus everything that no
/// longer fits in it.
///
/// Overflow messages are parked in `backlog` instead of being dropped. Order is
/// preserved because [`SessionQueue::push`] stops using the channel as soon as a
/// backlog exists: every channel item is then older than every backlog item, so
/// the worker may drain the channel first and still hand messages to the CLI in
/// arrival order.
pub(super) struct SessionQueue {
    tx: mpsc::Sender<Queued>,
    backlog: std::sync::Mutex<VecDeque<Queued>>,
    notify: tokio::sync::Notify,
}

impl SessionQueue {
    fn new(tx: mpsc::Sender<Queued>) -> Self {
        Self {
            tx,
            backlog: std::sync::Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Never blocks and never drops: `try_send` while the backlog is empty,
    /// otherwise append to the backlog and wake the worker.
    fn push(&self, item: Queued) {
        let mut backlog = self.backlog.lock().unwrap_or_else(|e| e.into_inner());
        if !backlog.is_empty() {
            backlog.push_back(item);
            drop(backlog);
            self.notify.notify_one();
            return;
        }
        match self.tx.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(item)) => {
                backlog.push_back(item);
                drop(backlog);
                self.notify.notify_one();
            }
            // The worker is gone; the WAL keeps the message for the next run.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn pop_backlog(&self) -> Option<Queued> {
        self.backlog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session_worker(
    key: String,
    mut rx: mpsc::Receiver<Queued>,
    queue: Arc<SessionQueue>,
    client: Arc<dyn Transport>,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
    wal: Option<Wal>,
) {
    const SESSION_WORKER_MAX_BACKOFF_SECS: u64 = 60;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Yield immediately if shutdown was already requested: whatever is still
        // queued stays in the WAL and is answered by the next run.
        if shutdown.is_cancelled() {
            return;
        }
        // Oldest first: the channel, then the overflow backlog. Park only when
        // both are empty.
        let item = match rx.try_recv() {
            Ok(item) => item,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                info!(session_key = %key, "session worker exiting");
                return;
            }
            Err(mpsc::error::TryRecvError::Empty) => match queue.pop_backlog() {
                Some(item) => item,
                None => tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return,
                    msg_opt = rx.recv() => match msg_opt {
                        Some(item) => item,
                        None => {
                            info!(session_key = %key, "session worker exiting");
                            return;
                        }
                    },
                    _ = queue.notify.notified() => continue,
                },
            },
        };
        let Queued { msg, wal_id } = item;

        // Capture the routing identifiers needed for an error reply *before* moving `msg`
        // into `handle_one_message`, so that if the future is cancelled we can still send
        // feedback to the user.
        let ctx_for_err = msg.context_token.clone().unwrap_or_default();
        let from_for_err = msg.from_user.clone().unwrap_or_default();
        // Capture the session name so the shutdown error reply carries the same session in
        // its ilink_hub_ext. Without this the Hub appends the *current active* session to
        // the footer and registers the error message under that session in the quote_index,
        // causing any quote-reply to that "响应中断" message to be routed to the wrong session.
        let session_name_for_err = msg.session_name.clone().filter(|s| !s.trim().is_empty());

        let result = tokio::select! {
            biased;
            // Shutdown arrived while we were processing — cancel the AI call and tell user.
            _ = shutdown.cancelled() => {
                if app.send_error_reply() && !ctx_for_err.is_empty() {
                    let reply = OutboundReply {
                        context_token: ctx_for_err,
                        text: "⚠️ 响应中断（服务正在重启），请稍后重发消息".to_string(),
                        to_user: from_for_err,
                        session_name: session_name_for_err,
                        ..Default::default()
                    };
                    match client.send_reply(reply).await {
                        Ok(SendOutcome::Sent) => {}
                        Ok(SendOutcome::Throttled { ret, errmsg }) => {
                            // User did not receive the "service restarting,
                            // please resend" notice. This is an
                            // observability hit for the user. M3 must
                            // include this path when wiring buffer+retry.
                            warn!(
                                ret,
                                errmsg = sanitize_errmsg(errmsg.as_deref()).as_deref(),
                                "sendmessage throttled during shutdown error reply; user did NOT receive restart notice — M3 must cover this path when adding buffer+retry"
                            );
                        }
                        Err(e) => warn!(error = %e, "failed to send shutdown error reply"),
                    }
                }
                return;
            }
            r = handle_one_message(&client, &app, msg, shutdown.clone()) => r,
        };

        match result {
            Ok(()) => {
                consecutive_failures = 0;
                // The reply (or the decision to ignore the message) landed: the
                // WAL entry is no longer needed for crash recovery.
                if let (Some(wal), Some(id)) = (wal.as_ref(), wal_id.as_deref()) {
                    wal.complete(id);
                }
            }
            Err(HandleError::Fatal(reason)) => {
                error!(session_key = %key, reason = ?reason, "fatal CLI error; signalling bridge stop");
                let _ = stop_tx.send(Some(reason));
                return;
            }
            Err(HandleError::Transient(e)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff_secs =
                    SESSION_WORKER_MAX_BACKOFF_SECS.min(1_u64 << consecutive_failures.min(63));
                error!(
                    session_key = %key,
                    error = %e,
                    consecutive_failures,
                    backoff_secs,
                    "message handler failed; backing off before next message"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
        }
    }
}

pub(super) enum HandleError {
    Transient(anyhow::Error),
    Fatal(BridgeStop),
}

impl From<anyhow::Error> for HandleError {
    fn from(e: anyhow::Error) -> Self {
        HandleError::Transient(e)
    }
}

const DEFAULT_SESSION_QUEUE_SIZE: usize = 200;
/// Maximum number of concurrent session workers. Each worker holds an open mpsc channel and a
/// spawned Tokio task; unbounded growth would exhaust both. When the cap is reached, the oldest
/// idle (closed-channel) entries are evicted first; if all entries are still active the new
/// message is dropped with a warning.
const MAX_SESSION_WORKERS: usize = 512;

pub(super) struct SessionDispatcher {
    // std::sync::Mutex is correct here: the critical section contains only synchronous
    // HashMap operations (retain/get/insert) with no await points.
    pub(super) senders: std::sync::Mutex<HashMap<String, Arc<SessionQueue>>>,
    client: Arc<dyn Transport>,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    shutdown: CancellationToken,
    /// Durable log of messages that were taken off the transport but not answered
    /// yet. `None` when the WAL could not be opened: the bridge still runs, but a
    /// crash loses in-flight messages again.
    wal: Option<Wal>,
    /// Cumulative count of messages that could not be dispatched because the
    /// MAX_SESSION_WORKERS cap was reached. With a WAL attached the message is
    /// on disk and is redelivered by the next run.
    /// Visible in structured logs via the warn! on each drop; exposed in the bridge
    /// metrics endpoint (TODO: requires a bridge-side HTTP server).
    sessions_dropped_on_cap: Arc<AtomicU64>,
}

impl SessionDispatcher {
    pub(super) fn new(
        client: Arc<dyn Transport>,
        app: Arc<BridgeApp>,
        stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            senders: std::sync::Mutex::new(HashMap::new()),
            client,
            app,
            stop_tx,
            shutdown,
            wal: None,
            sessions_dropped_on_cap: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach the write-ahead log this dispatcher records to / completes
    /// against. Without one (tests, or an unopenable WAL directory) dispatch
    /// still works, it just has no crash recovery.
    pub(super) fn with_wal(mut self, wal: Option<Wal>) -> Self {
        self.wal = wal;
        self
    }

    pub(super) async fn dispatch(&self, msg: InboundMessage) {
        // Record *before* the message enters the queue: from here on the
        // transport cursor has already been advanced past it, so disk is the
        // only thing standing between a crash and a lost user message.
        let wal_id = self.wal.as_ref().and_then(|wal| wal.record(&msg));
        self.enqueue(msg, wal_id);
    }

    /// Re-dispatch a message recovered from the WAL. It is already on disk, so
    /// the original entry id travels with it and is removed once answered.
    pub(super) fn replay(&self, msg: InboundMessage, wal_id: String) {
        self.enqueue(msg, Some(wal_id));
    }

    fn enqueue(&self, msg: InboundMessage, wal_id: Option<String>) {
        let key = session_dispatch_key(&msg);

        // N-04 / F-M1-N04: match the poison-safe style used by
        // `evict_closed_senders` below — recover from a poisoned mutex by
        // taking the inner state instead of panicking the Tokio worker.
        let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());

        // Check if a live worker already exists for this key.
        let needs_new = match senders.get(&key) {
            Some(queue) => queue.tx.is_closed(),
            None => true,
        };

        if needs_new {
            // Evict closed entries only when we need to make room — avoids O(N)
            // retain on every message. The background evict_closed_senders task
            // handles periodic cleanup so the map doesn't grow unbounded.
            if senders.len() >= MAX_SESSION_WORKERS {
                senders.retain(|_, queue| !queue.tx.is_closed());
                if senders.len() >= MAX_SESSION_WORKERS {
                    let total_dropped =
                        self.sessions_dropped_on_cap.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        session_key = %key,
                        cap = MAX_SESSION_WORKERS,
                        active = senders.len(),
                        sessions_dropped_on_cap = total_dropped,
                        wal = self.wal.is_some(),
                        "session worker cap reached, not dispatching this message"
                    );
                    return;
                }
            }
            let (tx, rx) = mpsc::channel(DEFAULT_SESSION_QUEUE_SIZE);
            let queue = Arc::new(SessionQueue::new(tx));
            senders.insert(key.clone(), Arc::clone(&queue));
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            let stop_tx = self.stop_tx.clone();
            let shutdown = self.shutdown.clone();
            let wal = self.wal.clone();
            tokio::spawn(run_session_worker(
                key.clone(),
                rx,
                queue,
                client,
                app,
                stop_tx,
                shutdown,
                wal,
            ));
        }

        if let Some(queue) = senders.get(&key) {
            queue.push(Queued { msg, wal_id });
        }
    }

    /// Remove closed sender entries. Called periodically by a background task
    /// so the map doesn't accumulate dead entries between cap-enforcement evictions.
    pub(super) fn evict_closed_senders(&self) {
        if let Ok(mut senders) = self.senders.lock() {
            senders.retain(|_, queue| !queue.tx.is_closed());
        }
    }

    #[cfg(test)]
    pub(super) fn sender_keys(&self) -> Vec<String> {
        // N-04: tests use the same poison-safe recovery as production code
        // so the helper stays consistent with `dispatch` and
        // `evict_closed_senders`.
        let mut keys: Vec<String> = self
            .senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}
