//! issue #8 契约回归：长回复按通道文本上限切分 + 确定性 4xx 停止重试。
//!
//! 状态：**已修复**。用例在修复前为 RED（以 `#[ignore]` 隔离），修复后去掉
//! `#[ignore]` 转为常驻契约回归，随默认 `cargo test` 一起跑。
//!
//! 文件名未用字面 `[wip]` 前缀：cargo 会把它当作 crate name 直接报
//! `invalid character '[' in crate name`，故退化为 `wip_` 前缀（历史遗留，
//! 改文件名需要同步 CI/管线，成本大于收益）。
//!
//! 为什么用源码字符串断言：`bridge::dispatcher` 是 `pub(crate)`，
//! `TelegramTransport` / `DiscordTransport` 等 adapter 类型位于
//! `pub(crate) mod`，集成测试既无法命名它们，也无法调用
//! `executor::split_into_parts`。真正的行为断言落在 crate 内单测：
//! `src/bridge/dispatcher/tests.rs`（ScriptedSender 已具备 capabilities 桩）、
//! `src/bridge/transport/{telegram,discord,feishu}.rs` 的 `#[cfg(test)]`（mockito）。
//! 本文件只做「契约存在性」回归，风格沿用本仓既有的 `tests/doc_claims.rs`。

const TRANSPORT: &str = include_str!("../src/bridge/transport.rs");
const TELEGRAM: &str = include_str!("../src/bridge/transport/telegram.rs");
const DISCORD: &str = include_str!("../src/bridge/transport/discord.rs");
const FEISHU: &str = include_str!("../src/bridge/transport/feishu.rs");
const WECOM: &str = include_str!("../src/bridge/transport/wecom.rs");
const ILINK: &str = include_str!("../src/bridge/transport/ilink.rs");
const HANDLE: &str = include_str!("../src/bridge/dispatcher/handle.rs");
const SEND: &str = include_str!("../src/bridge/dispatcher/send.rs");

#[test]
fn capabilities_declare_max_text_len() {
    assert!(
        TRANSPORT.contains("max_text_len"),
        "TransportCapabilities 缺少 max_text_len 字段（src/bridge/transport.rs:184）"
    );
    assert!(
        TELEGRAM.contains("max_text_len: Some(4096)"),
        "telegram 未声明 4096（telegram.rs:533 capabilities）"
    );
    assert!(
        DISCORD.contains("max_text_len: Some(2000)"),
        "discord 未声明 2000（discord.rs:671 capabilities）"
    );
    for (name, src) in [
        ("wecom", WECOM),
        ("feishu", FEISHU),
        ("ilink", ILINK),
        ("transport.rs(test)", TRANSPORT),
    ] {
        assert!(
            !src.contains("TransportCapabilities { media_upload: true }"),
            "{name}: 仍有未同步新字段的字面量构造点（加字段后应先编译失败再补齐）"
        );
    }
}

#[test]
fn dispatcher_splits_by_min_of_profile_and_transport_limit() {
    assert!(
        HANDLE.contains("max_text_len"),
        "handle.rs:246 仍只按 profile.max_reply_chars 切分，未取 transport 上限"
    );
    assert!(
        HANDLE.contains("min("),
        "切分长度应为 min(profile.max_reply_chars, capabilities().max_text_len)"
    );
}

#[test]
fn length_4xx_is_distinguishable_and_not_retried() {
    assert!(
        TRANSPORT.contains("Rejected"),
        "SendOutcome 缺少可区分的拒绝结果（如 Rejected{{ret, errmsg}}）"
    );
    assert!(
        TELEGRAM.contains("Rejected"),
        "telegram.rs:387 send_message 未把 400 message is too long 映射为 Rejected"
    );
    assert!(
        DISCORD.contains("Rejected"),
        "discord.rs:580 send_reply 未把 400 Invalid Form Body 映射为 Rejected"
    );
    assert!(
        SEND.contains("SendOutcome::Rejected"),
        "send.rs 未处理 Rejected：确定性 4xx 仍会走到 60-300s 退避重试"
    );
    assert!(
        FEISHU.contains("Rejected"),
        "feishu 99991400(message too long) 仍被当成 Throttled 重试"
    );
}
