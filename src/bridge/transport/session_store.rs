//! 非 iLink 通道的跨轮 CLI 会话号本地 store。
//!
//! 键 = `(transport 名, 适配器定义的会话键)`，值 = 上一轮 agentproc 回传的
//! `cli_session_id`。适配器在 `send_reply` 里写入回包的 `cli_session_id`，
//! 在下一轮的入站转换里读回，填进 `InboundMessage.session_id`。
//!
//! - **进程内**：本 store 只是一张 `HashMap`，bridge 重启后非 iLink 通道从冷会话
//!   重新开始。iLink 的持久化仍由 Hub 负责（`ilink.rs` 不动）。
//! - **会话键由适配器定义**：telegram / feishu / discord 用 `context_token`
//!   （chat_id / chat_id / channel_id）；wecom 的 `context_token` 是**每条消息一个**
//!   的 `req_id`（回复必须逐字回填），所以 wecom 用 `chatid` 作会话键，另在适配器内
//!   维护 `req_id → chatid` 映射。
//! - **键含 transport 名**：同一进程里 telegram 的 chat_id 与 discord 的 channel_id
//!   可能撞值，不能互串。

use std::collections::HashMap;

/// 进程内的 `(transport, 会话键) → cli_session_id` 表。
#[derive(Default)]
pub(crate) struct SessionStore {
    entries: std::sync::Mutex<HashMap<(String, String), String>>,
}

impl SessionStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn lookup(&self, transport: &str, conv_key: &str) -> Option<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(&(transport.to_string(), conv_key.to_string()))
            .cloned()
    }

    /// `None` / 空白串一律忽略：既不写入，也不覆盖已存值（错误回包与「仅持久化」
    /// 回包并存时的正确语义）。
    pub(crate) fn remember(&self, transport: &str, conv_key: &str, cli_session_id: Option<&str>) {
        let Some(session_id) = cli_session_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return;
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            (transport.to_string(), conv_key.to_string()),
            session_id.to_string(),
        );
    }
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
}
