# WeCom (Enterprise WeChat)

This guide shows how to connect a WeCom (企业微信) Intelligent Robot to an agentproc profile using the `wecom` transport.

## How it works

The `wecom` transport uses the **WeCom AI Bot WebSocket long-connection API** (`/cgi-bin/aibot/connect`). The bridge subscribes to inbound messages via `aibot_subscribe`, handles `aibot_msg_callback` events, and replies with `aibot_respond_msg` — all over the same persistent WebSocket.

> **Note:** This uses the [WeCom Intelligent Robot (AI Bot)](https://developer.work.weixin.qq.com/document/path/99654) API, which is available in the **公共版** (standard) enterprise plan. It is separate from the older "企业机器人" group-message webhook.

## Prerequisites

- A WeCom (企业微信) account with admin access to a corporation
- `im-agentproc` installed

## 1. Create an AI Bot application

1. Log in to the [WeCom admin console](https://work.weixin.qq.com/wework_admin/frame)
2. Navigate to **应用管理 → 应用 → 创建应用 → AI Bot**
3. Fill in the bot name and description, upload an avatar
4. After creation, go to the bot settings and note:
   - **Bot ID** (`bot_id`) — shown as "机器人 ID"
   - **Bot Secret** (`bot_secret`) — shown as "密钥"
5. Set the **API receive mode** to "消息接收模式 → 企业微信主动连接（长连接）" (Long-connection mode)
6. Enable the scopes your bot needs (e.g., send/receive messages in group chats or 1-on-1)

## 2. Write the profile YAML

```yaml
description: My WeCom AI coding assistant
transport: wecom
im_credentials:
  bot_id: ${WECOM_BOT_ID}
  bot_secret: ${WECOM_BOT_SECRET}
agentproc:
  executor: claude-code
```

Set environment variables before running:

```bash
export WECOM_BOT_ID=your-bot-id
export WECOM_BOT_SECRET=your-bot-secret
```

## 3. Run the bridge

```bash
WECOM_BOT_ID=your-bot-id \
WECOM_BOT_SECRET=your-bot-secret \
  im-agentproc --config ~/.im-agentproc/wecom-profile.yaml
```

The bridge connects to `wss://openai.work.weixin.qq.com/openai/connect/...`, subscribes to incoming messages, and begins forwarding them to the agentproc profile.

## Notes

- **Only text messages** are handled in this version. Attachment/image callbacks are ignored.
- **Anti-loop**: Bot-originated messages are skipped.
- **Session continuity**: `session_id` is the WeCom conversation ID (`open_kfid + external_userid` for customer service bots, or the chat ID for group bots), so each conversation has independent CLI session tracking.
- **Reconnect**: The WebSocket worker reconnects with exponential backoff (5s → 60s cap) on disconnect.
- **Token refresh**: `access_token` and `aibot_ticket` are refreshed automatically before expiry.

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `transport: wecom 需要 im_credentials.bot_id` | Env vars not set and `im_credentials` absent |
| WS connect error `40001` | `bot_secret` is wrong |
| WS connect error `40013` | `bot_id` is invalid or the bot does not have long-connection mode enabled |
| No replies sent | Check agentproc profile logs; also verify the bot has reply permissions in the admin console |
