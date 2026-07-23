---
layout: home

hero:
  name: IM-AgentProc
  text: Bridge an IM transport to local coding CLIs
  tagline: An agentproc-native IM runtime. Each inbound IM message triggers one agentproc profile run — no HTTP, no sockets, just stdin and stdout.
  actions:
    - theme: brand
      text: Quick Start
      link: /guide/quickstart
    - theme: alt
      text: What is IM-AgentProc?
      link: /guide/what-is-im-agentproc

features:
  - icon: 💬
    title: IM → AgentProc, one message one run
    description: Connects an IM transport (iLink/WeChat today) to agentproc profiles. Every inbound text message drives exactly one agentproc profile run, with CLI session continuity across turns.
  - icon: 🔌
    title: Virtual-token backend for iLink Hub
    description: Registers as a backend of iLink Hub via /hub/register, gets a virtual token, and long-polls inbound messages — no manual bot_token provisioning.
  - icon: 🧩
    title: Pluggable Transport trait
    description: The dispatcher speaks only generic IM DTOs. iLink is the only real adapter today; Feishu / Telegram / … land as new Transport implementations without touching the dispatcher.
  - icon: 🤖
    title: Built-in P0 exec profiles
    description: Ships built-in handlers for claude-code, codex, cursor, codebuddy-code, agy, recursive — each follows the agentproc 0.4 agent contract (stdin turn → stdout NDJSON).
  - icon: 🗂️
    title: One file, one profile — or a manager
    description: Run a single bridge YAML, or `im-agentproc manager` to supervise one child bridge per YAML in a profiles directory, each registering as an independent Hub backend.
  - icon: 🛡️
    title: Safe by construction
    description: Profiles are pure agentproc P0. Shell-`-c` + `{{MESSAGE}}` injection is rejected at load time; the message always travels via the stdin turn object, never via argv.
---

<div class="get-started">

## How it fits

IM-AgentProc was extracted from `ilink-hub`'s `src/bridge/` subtree. It is the agentproc-native successor of the bridge that lived inside the hub: same runtime behavior, now a standalone crate so it can grow multi-IM support without bloating the hub.

```
WeChat user → iLink → iLink Hub → IM-AgentProc (vtoken backend) → agentproc profile (claude-code / codex / …) → reply
```

## Install

::: code-group

```bash [cargo]
cargo install im-agentproc
```

```bash [brew]
brew tap jeffkit/tap
brew install im-agentproc
```

```bash [from source]
git clone https://github.com/jeffkit/im-agentproc
cd im-agentproc
cargo build --release
# binary at target/release/im-agentproc
```

:::

Verify:

```bash
im-agentproc --version
```

## Run in one command

Point the bridge at your iLink Hub and a profile YAML:

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765   # your iLink Hub
im-agentproc --config ~/.ilink-hub/ilink-hub-bridge.yaml
```

The bridge auto-registers with the Hub (no scan needed by default), gets a virtual token, and starts long-polling for inbound WeChat messages. Each message runs the configured agentproc profile and the reply is sent back through the Hub.

## Where to go next

- **[What is IM-AgentProc?](/guide/what-is-im-agentproc)** — positioning and the ilink-hub split
- **[Quick Start](/guide/quickstart)** — install, configure, first message
- **[Configuration](/guide/configuration)** — every bridge YAML field
- **[CLI reference](/cli)** — default mode, `profile`, `manager`
- **[Built-in profile spec](/bridge/profile-spec)** — write your own profile handler
- **[Transport extension](/transport)** — add Feishu / Telegram

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
