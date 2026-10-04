//! issue #10 契约回归（复现）：降级路径必须把 `cli_session_id` 挂到该轮最后一个成功投递的子片。
//!
//! 状态：**RED（未修复）**。修复后本文件应转绿并常驻为契约回归。
//!
//! 文件名未用字面 `[wip]` 前缀：cargo 会把它当作 crate name 直接报
//! `invalid character '[' in crate name`，故退化为 `wip_` 前缀（沿用
//! `tests/wip_issue8_text_limits.rs` 的既有约定）。
//!
//! 为什么用源码字符串断言：`bridge::dispatcher` 与其 `send` 子模块都是
//! `pub(crate)`，`send_final_parts` 是 `pub(super)`，集成测试既无法命名
//! `ScriptedSender` 也无法调用 `send_final_parts`。真正的行为断言（脚本化
//! Transport：末段被拒 → 拆半 → 断言仅最后一个子片带会话号）只能落在
//! crate 内单测 `src/bridge/dispatcher/tests.rs`。本文件只做「契约存在性」
//! 回归，风格沿用 `tests/wip_issue8_text_limits.rs`。

const SEND: &str = include_str!("../src/bridge/dispatcher/send.rs");

/// Return the degraded sub-part loop region of `send_final_parts`: everything
/// from the `split_into_parts(&part, halved)` degradation entry point to EOF.
fn degraded_subpart_region(src: &str) -> &str {
    let marker = "let sub_parts = split_into_parts(&part, halved);";
    let start = src.find(marker).unwrap_or_else(|| {
        panic!("send.rs 未找到降级拆分入口 {marker:?}（issue #8 引入的降级路径）")
    });
    &src[start..]
}

#[test]
fn degraded_subpart_loop_propagates_cli_session() {
    let region = degraded_subpart_region(SEND);
    assert!(
        region.contains("cli_session.clone()"),
        "降级子片仍写死 `cli_session_id: None`（send.rs:385）——该类子片是这一轮最后的投递，\
         会话号不随行则 Hub 不持久化 CLI 会话号，下一条消息以冷会话起步（多轮上下文断链）"
    );
    let session_line = region
        .lines()
        .find(|l| l.contains("cli_session.clone()") && l.contains("None"))
        .unwrap_or_else(|| {
            panic!(
                "会话号必须以 `Option` 分支透传（仅最后一个成功投递的子片 Some/其余 None），\
                 而不是无条件挂到每个子片上"
            )
        });
    assert!(
        session_line.contains("if") || session_line.contains("then"),
        "会话号所在行未见分支：{}",
        session_line.trim()
    );
}

#[test]
fn degraded_subpart_session_is_gated_on_the_degraded_part_being_last() {
    let region = degraded_subpart_region(SEND);
    assert!(
        region.to_lowercase().contains("last"),
        "子片透传会话号必须以「被降级的 part 本身是 `is_last`」为闸门——\
         非末段 part 被降级时子片应全为 None（会话号不得提前/重复透传）"
    );
}
