---
layout: home

hero:
  name: IM-AgentProc
  text: 把 IM 传输桥接到本地编码 CLI
  tagline: agentproc-native 的 IM 运行时。每条入站 IM 消息触发一次 agentproc profile 运行——不用 HTTP、不用 socket，只用 stdin 和 stdout。
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/quickstart
    - theme: alt
      text: 什么是 IM-AgentProc？
      link: /zh/guide/what-is-im-agentproc

features:
  - icon: 💬
    title: IM → AgentProc，一条消息一次运行
    description: 把 IM 传输（当前 iLink/微信）桥接到 agentproc profile。每条入站文本消息驱动一次 agentproc profile 运行，并跨轮保持 CLI 会话续接。
  - icon: 🔌
    title: iLink Hub 的虚拟 token 后端
    description: 通过 /hub/register 注册为 iLink Hub 的后端，拿到虚拟 token，长轮询入站消息——无需手动申请 bot_token。
  - icon: 🧩
    title: 可插拔的 Transport trait
    description: dispatcher 只认通用 IM DTO。当前只有 iLink 一个真实适配器；飞书 / Telegram / …… 作为新的 Transport 实现接入，无需改动 dispatcher。
  - icon: 🤖
    title: 内置 P0 exec profile
    description: 内置 claude-code、codex、cursor、codebuddy-code、agy、recursive 处理器——每个都遵循 agentproc 0.4 agent 协议（stdin turn → stdout NDJSON）。
  - icon: 🗂️
    title: 一个文件一个 profile，或用 manager
    description: 跑单个 bridge YAML，或 `im-agentproc manager` 监管 profiles 目录下每个 YAML 各起一个子 bridge，各自注册为独立 Hub 后端。
  - icon: 🛡️
    title: 构造即安全
    description: profile 是纯 agentproc P0。shell-`-c` + `{{MESSAGE}}` 注入在加载时即被拒；消息始终经 stdin turn 对象传递，绝不走 argv。
---

<div class="get-started">

## 它在哪一层

IM-AgentProc 从 `ilink-hub` 的 `src/bridge/` 子树抽离而来。它是 hub 内那个 bridge 的 agentproc-native 后继：运行时行为一致，现在独立成 crate，以便扩展多 IM 支持时不撑大 hub。

```
微信用户 → iLink → iLink Hub → IM-AgentProc（虚拟 token 后端）→ agentproc profile（claude-code / codex / …）→ 回复
```

## 安装

::: code-group

```bash [cargo]
cargo install im-agentproc
```

```bash [brew]
brew tap jeffkit/tap
brew install im-agentproc
```

```bash [源码]
git clone https://github.com/jeffkit/im-agentproc
cd im-agentproc
cargo build --release
# 产物在 target/release/im-agentproc
```

:::

验证：

```bash
im-agentproc --version
```

## 一行命令跑起来

把 bridge 指向你的 iLink Hub 和一个 profile YAML：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765   # 你的 iLink Hub
im-agentproc --config ~/.ilink-hub/ilink-hub-bridge.yaml
```

bridge 自动注册到 Hub（默认无需扫码），拿到虚拟 token，开始长轮询入站微信消息。每条消息跑配置好的 agentproc profile，回复经 Hub 发回。

## 接下来去哪

- **[什么是 IM-AgentProc？](/zh/guide/what-is-im-agentproc)** —— 定位与 ilink-hub 拆分
- **[快速开始](/zh/guide/quickstart)** —— 安装、配置、第一条消息
- **[配置参考](/zh/guide/configuration)** —— bridge YAML 每个字段
- **[CLI 参考](/zh/cli)** —— 默认模式、`profile`、`manager`
- **[内置 Profile 规范](/zh/bridge/profile-spec)** —— 写自己的 profile 处理器
- **[Transport 扩展](/zh/transport)** —— 接飞书 / Telegram

</div>

<style>
.get-started {
  max-width: 880px;
  margin: 0 auto;
  padding: 40px 24px 60px;
}
.get-started h2 {
  margin-top: 48px;
  margin-bottom: 16px;
  padding-bottom: 8px;
  border-bottom: 1px solid var(--vp-c-divider);
  font-size: 1.4rem;
}
</style>
