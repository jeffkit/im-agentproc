# Telegram

This guide shows how to connect a Telegram Bot to an agentproc profile using the `telegram` transport.

## How it works

The `telegram` transport uses **HTTP long-polling** (`getUpdates`) — no public URL or webhook is needed. The bridge polls Telegram for new messages, runs the agentproc profile, and replies via `sendMessage`.

## Prerequisites

- A Telegram account
- `im-agentproc` installed

## 1. Create a bot

1. Open Telegram and message `@BotFather`
2. Send `/newbot` and follow the prompts (name, username)
3. Copy the **bot token** (format: `123456:ABCdef...`)

## 2. Write the profile YAML

```yaml
description: My AI coding assistant on Telegram
transport: telegram
im_credentials:
  token: ${TELEGRAM_BOT_TOKEN}
agentproc:
  executor: claude-code
```

`im_credentials.token` is expanded from the process environment at load time, so you can set it as an env var:

```bash
export TELEGRAM_BOT_TOKEN=123456:ABCdef...
```

Or inline it directly (not recommended for production):

```yaml
im_credentials:
  token: "123456:ABCdef..."
```

## 3. Run the bridge

```bash
TELEGRAM_BOT_TOKEN=123456:ABCdef... \
  im-agentproc --config ~/.im-agentproc/telegram-profile.yaml
```

Send a message to your bot in Telegram — the bridge picks it up, runs the profile, and sends the reply back.

## Notes

- **Only text messages are supported** in the initial implementation. Voice, photos, documents, etc. are ignored.
- **Anti-loop**: Messages sent by bots are dropped automatically.
- **Group chats**: The bot must be added to the group and either mentioned or replied to directly. The `session_id` is the chat ID, so each chat is an independent session.
- **Offset persistence**: The bridge keeps the `getUpdates` offset in memory. Restarting the bridge re-reads unacknowledged messages.

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| Bridge exits immediately with `transport: telegram 需要 im_credentials.token` | `TELEGRAM_BOT_TOKEN` env var not set and `im_credentials.token` absent |
| No response in Telegram | Check `RUST_LOG=im_agentproc=debug` for polling errors |
| `401 Unauthorized` in logs | Bot token is invalid or revoked — regenerate via `@BotFather /revoke` |
