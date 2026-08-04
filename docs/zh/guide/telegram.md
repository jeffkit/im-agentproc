# Telegram 接入

本指南介绍如何使用 `telegram` transport 将 Telegram Bot 接入 agentproc profile。

## 工作原理

`telegram` transport 使用 **HTTP 长轮询**（`getUpdates`）——无需公网 URL 或 Webhook。Bridge 轮询 Telegram 获取新消息，运行 agentproc profile，然后通过 `sendMessage` 发送回复。

## 前提条件

- 一个 Telegram 账号
- 已安装 `im-agentproc`

## 1. 创建 Bot

1. 打开 Telegram，搜索并私聊 `@BotFather`
2. 发送 `/newbot`，按提示填写机器人名称和用户名
3. 复制返回的 **Bot Token**（格式：`123456:ABCdef...`）

## 2. 编写 Profile YAML

```yaml
description: 我的 Telegram AI 编程助手
transport: telegram
im_credentials:
  token: ${TELEGRAM_BOT_TOKEN}
agentproc:
  executor: claude-code
```

`im_credentials.token` 在加载时从环境变量展开：

```bash
export TELEGRAM_BOT_TOKEN=123456:ABCdef...
```

## 3. 启动 Bridge

```bash
TELEGRAM_BOT_TOKEN=123456:ABCdef... \
  im-agentproc --config ~/.im-agentproc/telegram-profile.yaml
```

在 Telegram 向 Bot 发送消息，Bridge 接收后运行 profile 并将回复发回。

## 注意事项

- **当前仅支持文本消息**。语音、图片、文档等类型会被忽略。
- **防循环**：Bot 自身发出的消息自动丢弃。
- **群聊**：需将 Bot 添加到群组。`session_id` 是 chat_id，每个会话独立维护 CLI 状态。
- **偏移量**：`getUpdates` 偏移量仅在内存中保持。重启 Bridge 会重新处理未确认的消息。

## 常见问题

| 现象 | 可能原因 |
|------|---------|
| Bridge 启动时报 `transport: telegram 需要 im_credentials.token` | 未设置 `TELEGRAM_BOT_TOKEN` 环境变量且 `im_credentials` 为空 |
| Telegram 无响应 | 加 `RUST_LOG=im_agentproc=debug` 查看轮询日志 |
| 日志出现 `401 Unauthorized` | Bot Token 无效或已吊销，可通过 `@BotFather /revoke` 重新生成 |
