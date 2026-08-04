# im-agentproc 卫生铁律

> 本文件被 pge flow 的 Generator/repair prompt 自动读取并注入（见 pge.flow.js 的 `loadHygiene`）。
> 改动这里 = 改变 Generator 在本仓写代码时遵守的规矩。

## Rust 工程铁律（违反即视为失败）

- **新模块必须注册**：创建 `src/<mod>/` 必须同时加 `src/<mod>/mod.rs` 且在
  `src/lib.rs`（或 `src/main.rs`）里 `pub mod <mod>;`——否则 cargo 根本不编译。
- **禁止用 `cargo run` / `cargo build` 创建临时探查二进制**。要探查类型用
  `cargo expand` 或写到 `#[cfg(test)] mod tests` 里。worktree git status 必须干净。
- **涉及外部 crate 时必须先读源码确认 API 形状**：真实定义在
  `~/.cargo/registry/src/*/crate-name-*/src/` 下，不许凭印象写字段名/方法签名。
- **Cargo.toml 新依赖**：参考已有 optional 依赖的写法。`cargo update` 后必须将
  `Cargo.lock` 一并提交（publish.yml 用 Cargo.lock 保证可复现构建）。
- **编译验证**：实现完成后先跑 `cargo build`（或 `cargo check --all-targets`）
  确认编译过再交回——在质量门之前发现编译错，省一次 evaluator 调用。

## 本仓特定规矩（来自 AGENTS.md）

- **Transport 接缝**：新增 IM 只需实现 `Transport` trait（`next_inbound` /
  `send_reply` / `send_media` / `name` / `capabilities`），**不要动 dispatcher**。
- **`via: hub`（默认）vs `via: direct`**：hub 经 `/hub/register` 拿虚拟 token；
  direct 直连真实 iLink 上游（需 `base_url:`，拒绝 localhost Hub 默认值）。
- **出站媒体走 MCP tools**：`send_text` / `send_image` / `send_file` /
  `send_voice`；success silent、failure loud；二进制走 `data:` / `file://` /
  `https://` URI。`Transport::send_media()` 默认返 `Err` 让未实现的 adapter
  安全降级，`TransportCapabilities::media_upload` 决定 tool 路由。
- **安全**：消息始终经 stdin turn 对象传递；shell-`-c` + `{{MESSAGE}}`
  （args 或 env）加载即拒。
- **生产路径禁止裸 `unwrap()`**：用 thiserror + `?` 传播错误。测试代码里可用。
- **clippy 零 warning 容忍**：`-D warnings`，提交前必须全部清除。
- **入站附件规范化**：`attachments.rs` 约束 kind ∈ {image,file,audio,video,other}、
  url ∈ {file,http,https,data}、相对 `file://` 按 cwd 解析。改动附件处理时
  必须保持这套约束。
