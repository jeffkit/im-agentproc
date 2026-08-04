# AGENTS.md — im-agentproc

> IM-AgentProc：agentproc-native 的 IM 桥接运行时（从 ilink-hub 的 `src/bridge` 抽离）。
> 负责人：jeffkit | 创建：2026-07-20 | 最后更新：2026-07-27 (MCP outbound + send_media)

## 项目概述

把 IM 传输（iLink/微信、Telegram、WeCom、飞书、Discord；经 `Transport` trait 可扩展更多）桥接到 agentproc profile——**每条入站 IM 消息触发一次 agentproc profile 运行**。作为虚拟 token 后端连 iLink Hub，跑遵循 P0 exec 协议的内置 profile（claude-code/codex 等）。

**技术栈：** Rust, Tokio, agentproc (crates.io 0.11+), CLI, NDJSON
**主仓库：** `git@github.com:jeffkit/im-agentproc.git`
**文档站：** `docs/`（VitePress，中英双语）

## 架构地图

- `src/bin/im-agentproc.rs` — CLI 入口；三种模式：默认 bridge / `profile` / `manager`
- `src/bridge/` — bridge 核心
  - `config.rs` — bridge YAML 解析（`BridgeApp`/`BridgeProfile`/`Via`/`TransportKind`）；`im_credentials:` IM 凭据（支持 `${VAR}` 环境变量展开）；`script:` 简写展开；shell-`-c`+`{{MESSAGE}}` 注入拒绝
  - `dispatcher/` — 每条消息的运行循环（agentproc_runner / handle / send / session / backoff）
  - `builtin/` — 内置 profile 处理器（`im-agentproc profile <type>`）：claude-code / codebuddy-code / codex / cursor / agy / recursive + `common.rs`（P0 exec 样板）
  - `transport.rs` + `transport/` — IM 无关 `Transport` trait（`next_inbound` / `send_reply` / `send_media` / `name` / `capabilities`）；适配器：`IlinkTransport`（iLink/微信）、`TelegramTransport`（HTTP 长轮询）、`WecomTransport`（智能机器人 WebSocket）、`FeishuTransport`（WebSocket + HTTP）、`DiscordTransport`（Gateway WebSocket）；`NullTransport` 占位；`attachments.rs` 入站规范化（受控 kind 词表 + 限定 scheme + 相对 `file://` 解析）；`media.rs` 共享 `read_media_bytes` + `filename_from_url` 给所有 adapter
  - `manager.rs` — 多 profile 监管（每 YAML 一个子 bridge）
  - `protocol.rs` — agentproc 0.4 turn/事件类型（与 agentproc crate 对齐）
  - `vtoken_env.rs` — 从凭证文件读 vtoken
- `src/mcp/` — MCP stdio server（`mcp-server` 子命令入口）；暴露 4 个出站 tool（`send_text` / `send_image` / `send_file` / `send_voice`），silent 成功 / loud 失败；agent 通过 hub profile 的 `mcp_servers` 块连进来
  - `probe.rs` — 启动时 CLI 探测
- `src/ilink/` — iLink 登录/配对/类型
- `src/client/` — Hub 客户端（pairing）
- `src/paths.rs` — 规范用户数据路径（`~/.ilink-hub/`、`~/.ilink-hub-bridge/`）
- `examples/smoke_agentproc.rs` — agentproc::run + claude-code executor 冒烟
- `scripts/deploy-local-brew.sh` — 本机 brew 调试部署（不动公共 tap）

## 关键设计

- **一个文件一个 profile**：bridge YAML 是 spec 对齐的 hub 形式（`agentproc:` 块 + ilink-hub 兄弟字段）。无路由、无多 profile map（单 profile 模式文本原样透传）。
- **`via: hub`（默认）vs `via: direct`**：hub 经 `/hub/register` 拿虚拟 token；direct 直连真实 iLink 上游（需 `base_url:`，拒绝 localhost Hub 默认值，不支持跨消息 CLI 会话续接）。
- **内置 profile 即 agentproc 0.4 agent**：从 stdin 读 turn、向 stdout 写 NDJSON，和外部脚本/SDK 处理器同一契约。续接失败回退新会话。
- **Transport 接缝**：dispatcher 只认通用 IM DTO（`InboundMessage`/`OutboundReply`）；`session_id`/`session_name`/`a2a_call_id` 是 bridge 运行时字段，由适配器填充。新增 IM 只需实现 `Transport`，不动 dispatcher。
- **出站媒体走 MCP tools**：agent 通过 im-agentproc 暴露的 `send_text` / `send_image` / `send_file` / `send_voice` 投递内容；success silent、failure loud；二进制走 `data:` / `file://` / `https://` URI。`Transport::send_media()` 默认返 `Err` 让未实现的 adapter 安全降级，`TransportCapabilities::media_upload` 决定 tool 路由。
- **安全**：消息始终经 stdin turn 对象传递；shell-`-c` + `{{MESSAGE}}`（args 或 env）加载即拒。

## 开发约定

- 依赖 agentproc crate 走 crates.io（0.11+），不再 pin git rev。
- 改 bridge 行为后跑 `cargo test`；内置 profile 改动跑对应 `cargo run --example smoke_agentproc`。
- 本机调试部署用 `scripts/deploy-local-brew.sh`（产物仅本机，自动还原 tap formula）；公共 tap 发版需打 tag 并更新 formula 的 url/sha256。
- 文档站改动在 `docs/`（VitePress），中英双语保持同步；改完 `cd docs && npm run build` 验证。

## 常用命令

```bash
cargo build --release
cargo test
cargo run --example smoke_agentproc -- "reply with exactly: smoke ok"
im-agentproc --config ~/.ilink-hub/ilink-hub-bridge.yaml
im-agentproc manager --profiles-dir ~/.ilink-hub-bridge/profiles
scripts/deploy-local-brew.sh        # 本机 brew 部署
cd docs && npm install && npm run dev   # 文档站本地预览
```

## 与大仓的关系

本仓是 [infra4agent](https://github.com/jeffkit/infra4agent) 逻辑大仓（monarbor 管理）的子仓。跨仓架构与依赖边见大仓 `docs/ARCHITECTURE.md`（通道层）。本仓内导航以本文件为准。

## 深入阅读

| 文档 | 说明 |
|------|------|
| `README.md` | 项目主页（GitHub/crates.io） |
| `docs/` | VitePress 文档站（中英双语） |
| `docs/bridge/profile-spec.md` | 内置 profile P0 exec 规范 |
| `docs/transport.md` | Transport trait 扩展指南 |
