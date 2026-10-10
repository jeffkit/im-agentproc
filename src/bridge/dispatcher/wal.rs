//! Durable write-ahead log for inbound messages.
//!
//! Every transport hands messages over destructively: `getupdates` advances the
//! Hub cursor before the reply is sent, Telegram confirms a batch only through
//! the *next* poll's `offset`. Taking a message off the transport therefore used
//! to be an implicit acknowledgement, and a crash (or a dropped queue slot)
//! between "taken" and "answered" lost it for good.
//!
//! Each inbound message is now written to a fsynced file **before** it enters
//! the session queue, and that file is removed only after the reply was
//! confirmed delivered. [`Wal::pending`] is replayed when the bridge starts, so
//! delivery is at-least-once: a message can be answered twice after a crash, but
//! never silently dropped.
//!
//! Layout: one JSON file per message under the profile's WAL directory
//! (`$IM_AGENTPROC_WAL_DIR/<profile>/` or `~/.ilink-hub-bridge/wal/<profile>/`).
//! The file name is `{unix_millis}-{pid}-{seq}`, so a directory listing sorted by
//! name is the recording order.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::bridge::transport::InboundMessage;

/// On-disk shape of one WAL entry.
#[derive(Debug, Serialize, Deserialize)]
struct EntryFile {
    /// Unix millis at recording time; informational (also encoded in the name).
    recorded_at_ms: u64,
    message: InboundMessage,
}

/// A message recorded by an earlier run that was never answered.
pub(super) struct PendingEntry {
    pub(super) id: String,
    pub(super) recorded_at_ms: u64,
    pub(super) message: InboundMessage,
}

/// Handle to the WAL directory. Cheap to clone (one `Arc` per bridge).
#[derive(Clone)]
pub(super) struct Wal {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    seq: AtomicU64,
}

impl Wal {
    /// Open (creating if needed) the WAL directory `dir`.
    pub(super) fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        remove_stale_tmp_files(&dir);
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                seq: AtomicU64::new(0),
            }),
        })
    }

    /// Open the WAL of profile `scope`.
    ///
    /// The scope is always appended to the root so that profiles sharing a
    /// machine cannot replay each other's messages. Returns `None` (with a
    /// logged error) when the directory cannot be created: the bridge then runs
    /// without crash recovery instead of refusing to start at all.
    pub(super) fn open_default(scope: &str) -> Option<Self> {
        let root = match std::env::var("IM_AGENTPROC_WAL_DIR") {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
            _ => crate::paths::bridge_wal_dir(),
        };
        let dir = root.join(sanitize_scope(scope));
        match Self::open(dir.clone()) {
            Ok(wal) => Some(wal),
            Err(e) => {
                error!(
                    dir = %dir.display(),
                    error = %e,
                    "cannot open the inbound WAL; messages taken from the transport will not survive a crash"
                );
                None
            }
        }
    }

    /// Durably record `msg` and return its entry id.
    ///
    /// `None` when the entry could not be written. The caller still dispatches
    /// the message — answering it late beats dropping it — but it will not be
    /// recoverable after a crash.
    pub(super) fn record(&self, msg: &InboundMessage) -> Option<String> {
        let now = now_ms();
        let id = self.next_id(now);
        let path = self.inner.dir.join(format!("{id}.json"));
        let tmp = self.inner.dir.join(format!("{id}.tmp"));
        let entry = EntryFile {
            recorded_at_ms: now,
            message: msg.clone(),
        };
        let body = match serde_json::to_vec(&entry) {
            Ok(body) => body,
            Err(e) => {
                warn!(error = %e, "cannot serialize inbound message for the WAL");
                return None;
            }
        };
        match write_durable(&tmp, &path, &body) {
            Ok(()) => Some(id),
            Err(e) => {
                warn!(
                    dir = %self.inner.dir.display(),
                    error = %e,
                    "cannot write the inbound WAL entry; this message is not crash-recoverable"
                );
                let _ = std::fs::remove_file(&tmp);
                None
            }
        }
    }

    /// Drop the entry: its reply was confirmed delivered.
    pub(super) fn complete(&self, id: &str) {
        let path = self.inner.dir.join(format!("{id}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                path = %path.display(),
                error = %e,
                "cannot remove the answered WAL entry; it may be delivered again after a restart"
            ),
        }
    }

    /// Entries left behind by an earlier run, in recording order.
    pub(super) fn pending(&self) -> Vec<PendingEntry> {
        let dir = match std::fs::read_dir(&self.inner.dir) {
            Ok(dir) => dir,
            Err(e) => {
                warn!(dir = %self.inner.dir.display(), error = %e, "cannot read the inbound WAL");
                return Vec::new();
            }
        };
        let mut paths: Vec<PathBuf> = dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();

        let mut pending = Vec::with_capacity(paths.len());
        for path in paths {
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match read_entry(&path) {
                Ok(entry) => pending.push(PendingEntry {
                    id: id.to_string(),
                    recorded_at_ms: entry.recorded_at_ms,
                    message: entry.message,
                }),
                Err(e) => warn!(
                    path = %path.display(),
                    error = %e,
                    "unreadable WAL entry; leaving it in place"
                ),
            }
        }
        pending
    }

    fn next_id(&self, now: u64) -> String {
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        format!("{now:013}-{}-{seq:04}", std::process::id())
    }
}

fn read_entry(path: &Path) -> std::io::Result<EntryFile> {
    let body = std::fs::read(path)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write `body` to `tmp`, flush it to disk, then rename it onto `path`.
///
/// The rename is what makes a half-written entry impossible to observe: readers
/// only ever see complete files under names [`Wal::pending`] scans. The
/// directory sync afterwards is what makes the new *name* survive a power loss —
/// without it the entry exists in the page cache only.
fn write_durable(tmp: &Path, path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)?;
    sync_dir(path.parent())
}

#[cfg(unix)]
fn sync_dir(dir: Option<&Path>) -> std::io::Result<()> {
    match dir {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: Option<&Path>) -> std::io::Result<()> {
    Ok(())
}

/// Drop `.tmp` leftovers of a run that died mid-write.
fn remove_stale_tmp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_some_and(|ext| ext == "tmp") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Keep the profile name usable as a single path component.
fn sanitize_scope(scope: &str) -> String {
    let cleaned: String = scope
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    // `.` and `..` cannot survive the filter above as-is only if they are the
    // whole name; anything else maps to a literal directory name.
    if cleaned.is_empty() || cleaned.trim_matches('.').is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ctx: &str) -> InboundMessage {
        InboundMessage {
            context_token: Some(ctx.to_string()),
            from_user: Some("u1".to_string()),
            text: Some("hello".to_string()),
            session_name: Some("default".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn recorded_entry_survives_as_pending_until_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = Wal::open(dir.path().to_path_buf()).expect("open");

        let id = wal.record(&msg("ctx-1")).expect("record");
        let pending = wal.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].message.context_token.as_deref(), Some("ctx-1"));
        assert!(pending[0].recorded_at_ms > 0);

        wal.complete(&id);
        assert!(wal.pending().is_empty(), "an answered entry must be gone");
    }

    #[test]
    fn pending_returns_entries_in_recording_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = Wal::open(dir.path().to_path_buf()).expect("open");

        for ctx in ["ctx-1", "ctx-2", "ctx-3"] {
            wal.record(&msg(ctx)).expect("record");
        }

        let contexts: Vec<String> = wal
            .pending()
            .iter()
            .map(|e| e.message.context_token.clone().unwrap_or_default())
            .collect();
        assert_eq!(contexts, vec!["ctx-1", "ctx-2", "ctx-3"]);
    }

    #[test]
    fn a_reopened_wal_sees_entries_of_the_previous_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let wal = Wal::open(dir.path().to_path_buf()).expect("open");
            wal.record(&msg("ctx-crash")).expect("record");
        }

        let reopened = Wal::open(dir.path().to_path_buf()).expect("reopen");
        let pending = reopened.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message.text.as_deref(), Some("hello"));
    }

    #[test]
    fn partial_writes_are_not_replayed_and_are_cleaned_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = Wal::open(dir.path().to_path_buf()).expect("open");
        wal.record(&msg("ctx-ok")).expect("record");
        // A run killed mid-write leaves a .tmp behind; it must never be replayed.
        let leftover = dir.path().join("0000000000001-1-0000.tmp");
        std::fs::write(&leftover, b"{ truncated").expect("write tmp");

        let pending = wal.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message.context_token.as_deref(), Some("ctx-ok"));

        drop(wal);
        let reopened = Wal::open(dir.path().to_path_buf()).expect("reopen");
        assert!(!leftover.exists(), "stale .tmp must be cleaned on open");
        assert_eq!(reopened.pending().len(), 1);
    }

    #[test]
    fn scope_is_sanitized_into_a_single_path_component() {
        assert_eq!(sanitize_scope("default"), "default");
        assert_eq!(sanitize_scope("a/b c"), "a_b_c");
        assert_eq!(sanitize_scope("../.."), ".._..");
        assert_eq!(sanitize_scope(""), "default");
        assert_eq!(sanitize_scope(".."), "default");
    }
}
