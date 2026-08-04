# Feishu (Lark)

This guide shows how to connect a Feishu (飞书 / Lark) self-built app to an agentproc profile using the `feishu` transport.

## How it works

The `feishu` transport uses:
- **WebSocket long-connection** (via `larksuite-oapi-sdk-rs`) to receive `im.message.receive_v1` events without a public URL
- **Feishu IM HTTP API** (`POST /open-apis/im/v1/messages`) to send replies

> No webhook or public domain is needed. The SDK connects outbound to Feishu's WebSocket gateway.

## Prerequisites

- A Feishu / Lark account with admin access
- `im-agentproc` installed

## 1. Create a self-built app

1. Go to the [Feishu Open Platform](https://open.feishu.cn/app)
2. Click **创建企业自建应用** (Create self-built app) → fill in name and description
3. Go to **凭证与基础信息** (Credentials) and note:
   - **App ID** (`app_id`)
   - **App Secret** (`app_secret`)

## 2. Configure event subscription

1. In the app console, go to **事件订阅** (Event Subscription)
2. Set **请求地址配置方式** (Request mode) to **使用长连接接收事件** (Use long-connection to receive events) — this is the WebSocket mode that needs no public URL
3. Subscribe to the event: `im.message.receive_v1`

## 3. Add permissions (scopes)

In **权限管理** (Permission Management), add:
- `im:message` — read messages
- `im:message:send_as_bot` — send messages
- `im:message.file:download` — download files/images from messages (required for media support)

Then publish the app version for the scopes to take effect.

## 4. Install the app in the workspace

In **版本管理与发布** (Version & Release), publish the app (or request approval). Then install it to your workspace in **应用功能 → 机器人** (Bot feature).

## 5. Write the profile YAML

```yaml
description: My Feishu AI coding assistant
transport: feishu
im_credentials:
  app_id: ${FEISHU_APP_ID}
  app_secret: ${FEISHU_APP_SECRET}
agentproc:
  executor: claude-code
```

Set environment variables:

```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxx
export FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

## 6. Run the bridge

```bash
FEISHU_APP_ID=cli_xx... \
FEISHU_APP_SECRET=xx... \
  im-agentproc --config ~/.im-agentproc/feishu-profile.yaml
```

The bridge connects to Feishu's WebSocket gateway and starts receiving messages. Send a message to your bot in Feishu — the bridge processes it and replies in the same chat.

## Notes

- **Supported message types**: `text`, `image`, `file`, `audio`. Other types are silently dropped.
- **Media handling**: Images, files and audio are downloaded to local temp files via `GET /open-apis/im/v1/messages/{id}/resources/{key}?type=image|file`. The local path is forwarded to the agent as a `file://` URL in `MediaRef`. Media-only messages (no text) get a placeholder like `[image]` so the agent processes them.
- **Anti-loop**: Messages with `sender_type: bot` are skipped.
- **Session ID** is the `chat_id` of the Feishu conversation, providing independent CLI session continuity per chat.
- **Auto-reconnect**: The WS client has `auto_reconnect=true` and handles reconnections internally.
- **Token management**: `tenant_access_token` is refreshed automatically (5 min before expiry) and shared between the event handler and the send path.

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `transport: feishu 需要 im_credentials.app_id` | Env vars not set and `im_credentials` absent |
| `SDK client build failed` | `app_id` or `app_secret` format is invalid |
| Bot receives messages but sends no reply | Check that `im:message:send_as_bot` scope is granted and the app is installed |
| `tenant_access_token failed (code=10003)` | App Secret is wrong |
| Events not received | Verify "长连接接收事件" is selected in 事件订阅, and `im.message.receive_v1` is subscribed |
| Image/file download fails (403) | Add `im:message.file:download` scope to the app and republish |
