# Discord

This guide shows how to connect a Discord bot to an agentproc profile using the `discord` transport.

## How it works

The `discord` transport uses the **Discord Gateway WebSocket API**:
- Connects to `wss://gateway.discord.gg/?v=10&encoding=json`
- Sends an `IDENTIFY` payload (with `MESSAGE_CONTENT` intent)
- Maintains heartbeats and handles `RESUME` on reconnect
- Sends replies via the Discord REST API (`POST /channels/{channel_id}/messages`)

## Prerequisites

- A Discord account and a server (guild) where you have admin rights
- `im-agentproc` installed

## 1. Create a Discord application and bot

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications)
2. Click **New Application** → give it a name
3. Go to the **Bot** tab → click **Add Bot** → confirm
4. Under **Token**, click **Reset Token** and copy the bot token
5. Under **Privileged Gateway Intents**, enable:
   - **MESSAGE CONTENT INTENT** (required to read message text)

## 2. Invite the bot to your server

1. In the Developer Portal, go to **OAuth2 → URL Generator**
2. Select scopes: `bot`
3. Select bot permissions: `Send Messages`, `Read Message History`
4. Copy the generated URL and open it in a browser to add the bot to your server

## 3. Write the profile YAML

```yaml
description: My Discord AI coding assistant
transport: discord
im_credentials:
  token: ${DISCORD_BOT_TOKEN}
agentproc:
  executor: claude-code
```

Set the environment variable:

```bash
export DISCORD_BOT_TOKEN=Bot_Your_Token_Here
```

> **Note:** The token should be the raw bot token (without the `Bot ` prefix — the bridge adds it automatically for REST calls).

## 4. Run the bridge

```bash
DISCORD_BOT_TOKEN=your-token-here \
  im-agentproc --config ~/.im-agentproc/discord-profile.yaml
```

The bridge connects to the Discord Gateway, identifies with your bot token, and starts receiving `MESSAGE_CREATE` events. Send a message in any channel where the bot is present — it runs the agentproc profile and replies.

## Notes

- **Only text messages** (`MESSAGE_CREATE` with non-empty `content`) are handled. Bot messages are dropped (anti-loop).
- **Session ID** is the Discord channel ID — each channel has independent CLI session continuity.
- **Rate limits**: The Discord REST API enforces per-channel rate limits. The bridge does not implement retry-after handling in v1; if you send many messages quickly, some replies may fail.
- **Reconnect**: The WebSocket worker reconnects automatically using the `RESUME` sequence (with saved `session_id` and `seq`) when the gateway closes cleanly; falls back to a fresh connection on stale sessions.

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `transport: discord 需要 im_credentials.token` | Env var not set and `im_credentials` absent |
| Gateway connect error `4004` | Token is invalid |
| Gateway error `4014` | MESSAGE CONTENT INTENT not enabled in the Developer Portal |
| Bot is online but ignores messages | The bot might not have permission to read the channel |
| Replies fail with `403 Forbidden` | Bot lacks `Send Messages` permission in that channel |
