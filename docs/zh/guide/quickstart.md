# 快速开始

几分钟内从零跑起一个 IM → CLI bridge。

## 前置条件

- 一个**正在运行且可达的 iLink Hub**（默认 `http://127.0.0.1:8765`）。默认 `via: hub` 模式下 IM-AgentProc 是 Hub 的 *后端*，不是 Hub 的替代。
- 你的 profile 包装的**目标 CLI**，已安装并登录（如 `claude`、`codex`、`cursor` 等）。bridge 启动时会探测 CLI，缺失则直接退出。
- profile 所需的 API key，在 bridge 环境里可用（如 `ANTHROPIC_API_KEY`）。

## 1. 安装

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
# → target/release/im-agentproc
```

:::

验证：

```bash
im-agentproc --version
```

## 2. 写一个 bridge profile

bridge YAML 是**一个文件 == 一个 agentproc profile**，采用 spec 对齐的 hub 形式。创建 `~/.ilink-hub/ilink-hub-bridge.yaml`：

```yaml
description: claude-code on my project
agentproc:
  executor: claude-code          # in-process executor（agentproc SDK ≥ 0.10）
  cwd: ~/projects/my-app         # CLI 在哪跑
  streaming: true
  timeout_secs: 1800
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

就这样。`agentproc:` 块逐字段就是 agentproc profile 规范。没有 `type:` 快捷方式，没有 bridge 专属扩展。

::: tip 有 executor 时不需要 `command`
设了 `executor:` 且被 agentproc SDK 识别时，runner 直接驱动 CLI——`command` 可留空。没有 executor 时，`command`（或 `script:` 简写）必填。
:::

## 3. 跑 bridge

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765   # 你的 iLink Hub
export ANTHROPIC_API_KEY=sk-ant-...            # 经 ${ANTHROPIC_API_KEY} 取到
im-agentproc
```

首次运行无保存凭证时，bridge **自动注册**到 Hub（`POST /hub/register`），把虚拟 token 写到 `~/.ilink-hub/bridge-credentials.json`，开始长轮询。默认无需扫码。

::: tip Hub admin token
如果你的 Hub 开了 admin 鉴权（Hub 侧 `ILINK_ADMIN_TOKEN`），启动 bridge 前在同一环境设上该变量——否则自动注册会 HTTP 401 失败。
:::

## 4. 发一条消息

向 Hub 代理的微信账号发消息。bridge 会：

1. 从 Hub 收到消息
2. 构造 agentproc turn 对象并跑 profile（这里是 `claude-code`）
3. 把 `partial` 分片实时经 Hub 转发回去（`streaming: true` 时）
4. 发送最终回复

在同一微信会话里继续回复即可续接——CLI 的 session id 持久化在 Hub 上，下一轮自动续上。

## 5. 用 manager 跑多个 profile

要并行跑多个 profile、各自独立 Hub 后端：

```bash
# 每个 profile 一个 YAML 放进目录
mkdir -p ~/.ilink-hub-bridge/profiles
cp profile-a.yaml ~/.ilink-hub-bridge/profiles/
cp profile-b.yaml ~/.ilink-hub-bridge/profiles/

im-agentproc manager
```

manager 每 5s 扫一次 `~/.ilink-hub-bridge/profiles`，每个 YAML 起一个子 bridge（按文件名取名），崩溃的子进程按指数退避重启。每个子进程在 `~/.ilink-hub-bridge/credentials/` 下各有独立凭证 JSON。

## 其它连接方式

| 目的 | 命令 |
|------|------|
| 显式虚拟 token | `im-agentproc --token <vtoken>`（或 `WEIXIN_TOKEN`） |
| Hub 扫码配对（手机确认） | `im-agentproc --pair` |
| 保存的 token 失效后重新注册 | `im-agentproc --force-register` |
| 直连真实 iLink 上游 | YAML 里 `via: direct` + `base_url:`（见[什么是 IM-AgentProc？](/zh/guide/what-is-im-agentproc#两种连接模式)） |

## 故障排除

| 现象 | 修法 |
|------|------|
| `Startup probe failed for profile …` | 包装的 CLI 缺失或不在 `PATH`。安装/登录 CLI，或把 `command` 设成绝对路径。 |
| `WEIXIN_TOKEN / --token 被拒绝` | token 被吊销。重新注册：`im-agentproc --force-register`（hub）或 `--pair`（direct）。 |
| `CLI 认证失败` | CLI 自己的鉴权过期（如 `claude` 登出）。重新登录 CLI 并重启 bridge。 |
| 自动注册 HTTP 401 | Hub 开了 admin 鉴权。在 bridge 环境设 `ILINK_ADMIN_TOKEN`（与 Hub 一致）。 |
| `via: direct 需要显式 base_url` | direct 模式拒绝 localhost Hub 默认值。在 YAML 设 `base_url:` 或非默认 `WEIXIN_BASE_URL`。 |
| `transport … 没有真实适配器` | 你把 `transport:` 设成了 `ilink` 以外的值。当前只实现了 `ilink`；可插拔冒烟测试请加 `--allow-null-transport`。 |

下一篇：[配置参考](/zh/guide/configuration) →
