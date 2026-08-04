# Discord 接入

本指南介绍如何使用 `discord` transport 将 Discord Bot 接入 agentproc profile。

## 工作原理

`discord` transport 使用 **Discord Gateway WebSocket API**：
- 连接 `wss://gateway.discord.gg/?v=10&encoding=json`
- 发送 `IDENTIFY` payload（携带 `MESSAGE_CONTENT` intent）
- 维持心跳，断线时使用 `RESUME` 恢复会话
- 通过 Discord REST API（`POST /channels/{channel_id}/messages`）发送回复

## 前提条件

- 一个 Discord 账号，以及一个你拥有管理员权限的服务器（Guild）
- 已安装 `im-agentproc`

## 1. 创建 Discord 应用和 Bot

1. 打开 [Discord Developer Portal](https://discord.com/developers/applications)
2. 点击 **New Application** → 填写名称
3. 进入 **Bot** 标签页 → 点击 **Add Bot** → 确认
4. 在 **Token** 下点击 **Reset Token** 并复制 Bot Token
5. 在 **Privileged Gateway Intents** 下开启：
   - **MESSAGE CONTENT INTENT**（读取消息正文必须）

## 2. 将 Bot 邀请到服务器

1. 在 Developer Portal 进入 **OAuth2 → URL Generator**
2. 选择 Scopes：`bot`
3. 选择 Bot Permissions：`Send Messages`、`Read Message History`
4. 复制生成的链接，在浏览器中打开，将 Bot 添加到你的服务器

## 3. 编写 Profile YAML

```yaml
description: 我的 Discord AI 编程助手
transport: discord
im_credentials:
  token: ${DISCORD_BOT_TOKEN}
agentproc:
  executor: claude-code
```

设置环境变量：

```bash
export DISCORD_BOT_TOKEN=你的Bot_Token
```

> **注意：** Token 填原始值（不需要加 `Bot ` 前缀，Bridge 发送 REST 请求时会自动添加）。

## 4. 启动 Bridge

```bash
DISCORD_BOT_TOKEN=your-token-here \
  im-agentproc --config ~/.im-agentproc/discord-profile.yaml
```

Bridge 连接 Discord Gateway，开始接收 `MESSAGE_CREATE` 事件。在 Bot 所在的频道发送消息，Bridge 运行 agentproc profile 后回复。

## 注意事项

- **当前仅支持文本消息**（非空 `content` 的 `MESSAGE_CREATE`）。Bot 自身消息会被丢弃（防循环）。
- **会话 ID** 是 Discord 频道 ID，每个频道独立维护 CLI 会话状态。
- **频率限制**：Discord REST API 对每个频道有频率限制。v1 实现未对 `retry-after` 做重试处理，高频消息场景下个别回复可能失败。
- **自动重连**：网关正常关闭时会用 `RESUME` 序列（保留 `session_id` 和序号）恢复；会话失效时回退到重新连接。

## 常见问题

| 现象 | 可能原因 |
|------|---------|
| 报 `transport: discord 需要 im_credentials.token` | 未设置环境变量且 `im_credentials` 为空 |
| Gateway 连接报错 `4004` | Token 无效 |
| Gateway 报错 `4014` | Developer Portal 未开启 MESSAGE CONTENT INTENT |
| Bot 在线但不响应消息 | Bot 可能没有读取该频道的权限 |
| 回复失败 `403 Forbidden` | Bot 缺少该频道的 `Send Messages` 权限 |
