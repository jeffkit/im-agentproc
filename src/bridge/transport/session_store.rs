//! 非 iLink 通道的跨轮 CLI 会话号本地 store。
//!
//! 键 = `(transport 名, 适配器定义的会话键)`，值 = 上一轮 agentproc 回传的
//! `cli_session_id`。适配器在 `send_reply` 里写入回包的 `cli_session_id`，
//! 在下一轮的入站转换里读回，填进 `InboundMessage.session_id`。
//!
//! - **落盘**：`SessionStore::open(path)` 在启动时加载（每个 profile 一个文件，
//!   路径由 `crate::paths::session_store_path_for_profile` 给出），写入节流
//!   （首次写立即落盘，其后 1s 内合并）并走同目录临时文件 + rename 原子替换，
//!   因此 bridge 重启后仍能续接。文件缺失 / 内容损坏 / 目录不可写时只 `warn!`
//!   一次并退化为内存态，bridge 照常启动。`SessionStore::new()` 保持纯内存语义
//!   （不落盘、不建目录），MCP 出站子进程与单元测试走它。
//!   iLink 的持久化仍由 Hub 负责（`ilink.rs` 不动）。
//! - **会话键由适配器定义**：telegram / feishu / discord 用 `context_token`
//!   （chat_id / chat_id / channel_id）；wecom 的 `context_token` 是**每条消息一个**
//!   的 `req_id`（回复必须逐字回填），所以 wecom 用 `chatid` 作会话键，另在适配器内
//!   维护 `req_id → chatid` 映射。
//! - **键含 transport 名**：同一进程里 telegram 的 chat_id 与 discord 的 channel_id
//!   可能撞值，不能互串。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// 适配器构造用：`Some(path)` → 落盘 store，`None` → 纯内存 store。
pub(crate) fn session_store_for(path: Option<PathBuf>) -> Arc<SessionStore> {
    Arc::new(match path {
        Some(path) => SessionStore::open(path),
        None => SessionStore::new(),
    })
}

/// 两次落盘之间的最小间隔：突发写入（一轮里多个回包）只落盘一次。
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct StoreState {
    entries: HashMap<(String, String), String>,
    /// 有内存改动尚未写盘（被节流跳过，或上一次写盘失败）。
    dirty: bool,
    /// 上一次尝试落盘的时刻；`None` = 本实例还没写过盘。
    last_flush: Option<Instant>,
    /// 写盘失败已告警过：避免每轮重试都刷日志。
    warned: bool,
}

/// `(transport, 会话键) → cli_session_id` 表，可选落盘。
pub(crate) struct SessionStore {
    /// `None` = 纯内存（`new()` / MCP 子进程），永不触碰文件系统。
    path: Option<PathBuf>,
    state: Mutex<StoreState>,
}

impl SessionStore {
    /// 纯内存表：不加载、不落盘、不建目录。
    pub(crate) fn new() -> Self {
        Self {
            path: None,
            state: Mutex::new(StoreState::default()),
        }
    }

    /// 打开（或首次创建）落盘 store。任何加载失败都降级为空表，绝不 panic。
    pub(crate) fn open(path: PathBuf) -> Self {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => {
                match serde_json::from_slice::<HashMap<String, HashMap<String, String>>>(&bytes) {
                    Ok(nested) => nested
                        .into_iter()
                        .flat_map(|(transport, per_transport)| {
                            per_transport
                                .into_iter()
                                .map(move |(conv_key, cli_session_id)| {
                                    ((transport.clone(), conv_key), cli_session_id)
                                })
                        })
                        .collect(),
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "会话 store 文件损坏，按冷会话启动（内存态继续工作）"
                        );
                        HashMap::new()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "会话 store 文件不存在，从冷会话开始");
                HashMap::new()
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "会话 store 文件读取失败，按冷会话启动（内存态继续工作）"
                );
                HashMap::new()
            }
        };
        Self {
            path: Some(path),
            state: Mutex::new(StoreState {
                entries,
                dirty: false,
                last_flush: None,
                warned: false,
            }),
        }
    }

    pub(crate) fn lookup(&self, transport: &str, conv_key: &str) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .entries
            .get(&(transport.to_string(), conv_key.to_string()))
            .cloned()
    }

    /// `None` / 空白串一律忽略：既不写入，也不覆盖已存值（错误回包与「仅持久化」
    /// 回包并存时的正确语义）。写入后按 [`FLUSH_INTERVAL`] 节流落盘；同步函数，
    /// 体内没有 await（持有 std 锁不跨越 await）。
    pub(crate) fn remember(&self, transport: &str, conv_key: &str, cli_session_id: Option<&str>) {
        let Some(session_id) = cli_session_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return;
        };
        let should_flush = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.insert(
                (transport.to_string(), conv_key.to_string()),
                session_id.to_string(),
            );
            state.dirty = true;
            self.path.is_some()
                && state
                    .last_flush
                    .is_none_or(|t| t.elapsed() >= FLUSH_INTERVAL)
        };
        if should_flush {
            self.flush_locked();
        }
    }

    /// 把内存表原子落盘（同目录临时文件 + `rename`）。调用者**不得**持有
    /// `state` 锁；本函数全程持锁（无 await，`clippy::await_holding_lock` 不适用），
    /// 因此不会漏掉并发写入的脏标记。任何失败只 `warn!` 一次并返回 false，
    /// 内存态照常工作。
    fn flush_locked(&self) -> bool {
        let Some(path) = self.path.as_ref() else {
            return false;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // 先记下「尝试时刻」：写盘失败也不在同一次节流窗口里反复重写。
        state.last_flush = Some(Instant::now());

        let json = {
            let mut nested: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
            for ((transport, conv_key), cli_session_id) in &state.entries {
                nested
                    .entry(transport.as_str())
                    .or_default()
                    .insert(conv_key.as_str(), cli_session_id.as_str());
            }
            serde_json::to_vec(&nested)
        };
        let json = match json {
            Ok(json) => json,
            Err(e) => {
                warn_flush_failure(&mut state, path, &e);
                return false;
            }
        };

        if let Err(e) = write_atomically(path, &json) {
            warn_flush_failure(&mut state, path, &e);
            return false;
        }

        state.dirty = false;
        true
    }
}

/// 写盘失败只告警一次：之后每轮重试都静默，避免刷日志。
fn warn_flush_failure(state: &mut StoreState, path: &Path, err: &dyn std::fmt::Display) {
    if state.warned {
        return;
    }
    state.warned = true;
    warn!(
        path = %path.display(),
        error = %err,
        "会话 store 落盘失败，保留内存态（bridge 继续工作）"
    );
}

/// 同目录写临时文件再 `rename`，保证替换是原子的（写入中途崩溃不会留下半个文件）。
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

impl Drop for SessionStore {
    /// 补一次被节流跳过（或上次失败）的落盘，避免 `kill -9` 前的最后一轮丢失。
    fn drop(&mut self) {
        let dirty = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.dirty
        };
        if dirty {
            self.flush_locked();
        }
    }
}

/// 与目标同目录的临时文件名：同目录才能保证 `rename` 是原子替换。
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_before_any_write_is_none() {
        let store = SessionStore::new();
        assert_eq!(store.lookup("telegram", "chat-1"), None);
    }

    #[test]
    fn remembered_session_is_read_back() {
        let store = SessionStore::new();
        store.remember("telegram", "chat-1", Some("cli-1"));
        assert_eq!(store.lookup("telegram", "chat-1").as_deref(), Some("cli-1"));
    }

    #[test]
    fn blank_or_missing_session_id_is_ignored_and_does_not_clobber() {
        let store = SessionStore::new();
        store.remember("telegram", "chat-1", Some("cli-1"));
        store.remember("telegram", "chat-1", None);
        store.remember("telegram", "chat-1", Some(""));
        store.remember("telegram", "chat-1", Some("   "));
        assert_eq!(store.lookup("telegram", "chat-1").as_deref(), Some("cli-1"));
    }

    #[test]
    fn same_conv_key_on_different_transports_do_not_collide() {
        let store = SessionStore::new();
        store.remember("telegram", "123", Some("tg-1"));
        store.remember("discord", "123", Some("dc-1"));
        assert_eq!(store.lookup("telegram", "123").as_deref(), Some("tg-1"));
        assert_eq!(store.lookup("discord", "123").as_deref(), Some("dc-1"));
    }

    #[test]
    fn reopen_after_drop_returns_remembered_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.sessions.json");

        let store = SessionStore::open(path.clone());
        store.remember("telegram", "chat-1", Some("cli-7"));
        // 节流窗口内的第二次写入只标脏，靠 `Drop` 补落盘。
        store.remember("wecom", "chat-9", Some("cli-8"));
        drop(store);

        let reopened = SessionStore::open(path);
        assert_eq!(
            reopened.lookup("telegram", "chat-1").as_deref(),
            Some("cli-7")
        );
        assert_eq!(reopened.lookup("wecom", "chat-9").as_deref(), Some("cli-8"));
    }

    #[test]
    fn missing_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::open(dir.path().join("never-written.sessions.json"));
        assert_eq!(store.lookup("telegram", "chat-1"), None);
    }

    #[test]
    fn corrupt_json_degrades_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.sessions.json");
        std::fs::write(&path, b"not json{").expect("write corrupt store");

        let store = SessionStore::open(path);
        assert_eq!(store.lookup("telegram", "chat-1"), None);
        store.remember("telegram", "chat-1", Some("cli-1"));
        assert_eq!(store.lookup("telegram", "chat-1").as_deref(), Some("cli-1"));
    }

    #[test]
    fn io_failure_keeps_memory_and_does_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 父路径是普通文件 ⇒ `create_dir_all` 必失败（权限/目录类故障的代表）。
        let blocker = dir.path().join("afile");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let path = blocker.join("nested").join("store.sessions.json");

        let store = SessionStore::open(path);
        store.remember("telegram", "chat-1", Some("cli-1"));
        assert_eq!(store.lookup("telegram", "chat-1").as_deref(), Some("cli-1"));
        drop(store);
    }

    #[test]
    fn writes_are_throttled_but_the_latest_value_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("profile.sessions.json");
        let store = SessionStore::open(path.clone());
        store.remember("feishu", "chat-1", Some("cli-1"));
        store.remember("feishu", "chat-1", Some("cli-2"));
        drop(store);

        let reopened = SessionStore::open(path);
        assert_eq!(
            reopened.lookup("feishu", "chat-1").as_deref(),
            Some("cli-2")
        );
    }
}
