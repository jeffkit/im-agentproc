# 企业微信（WeCom）接入

本指南介绍如何使用 `wecom` transport 将企业微信智能机器人接入 agentproc profile。

## 工作原理

`wecom` transport 使用**企业微信 AI Bot WebSocket 长连接 API**（`/cgi-bin/aibot/connect`）。Bridge 通过 `aibot_subscribe` 订阅入站消息，处理 `aibot_msg_callback` 回调，并用 `aibot_respond_msg` 在同一 WebSocket 上回复——无需公网地址。

> **说明：** 本 transport 使用的是[企业微信智能机器人（AI Bot）](https://developer.work.weixin.qq.com/document/path/99654) API，适用于企业微信**公共版**（标准版）企业，与早期的"企业机器人"群消息 Webhook 不同。

## 前提条件

- 拥有企业微信管理员权限的企业账号
- 已安装 `im-agentproc`

## 1. 创建 AI Bot 应用

1. 登录[企业微信管理后台](https://work.weixin.qq.com/wework_admin/frame)
2. 进入 **应用管理 → 应用 → 创建应用 → AI Bot**
3. 填写机器人名称、描述，上传头像
4. 创建完成后，在机器人设置中记录：
   - **机器人 ID**（`bot_id`）
   - **密钥**（`bot_secret`）
5. 将 **API 接收模式** 设置为 "消息接收模式 → 企业微信主动连接（长连接）"
6. 开启所需的权限范围（如在群聊或单聊中收发消息）

## 2. 编写 Profile YAML

```yaml
description: 我的企业微信 AI 编程助手
transport: wecom
im_credentials:
  bot_id: ${WECOM_BOT_ID}
  bot_secret: ${WECOM_BOT_SECRET}
agentproc:
  executor: claude-code
```

运行前设置环境变量：

```bash
export WECOM_BOT_ID=你的机器人ID
export WECOM_BOT_SECRET=你的密钥
```

## 3. 启动 Bridge

```bash
WECOM_BOT_ID=your-bot-id \
WECOM_BOT_SECRET=your-bot-secret \
  im-agentproc --config ~/.im-agentproc/wecom-profile.yaml
```

Bridge 连接到 `wss://openai.work.weixin.qq.com/openai/connect/...`，订阅消息，并开始将消息转发给 agentproc profile。

## 注意事项

- **当前仅支持文本消息**。附件、图片等回调会被忽略。
- **防循环**：机器人自身发出的消息自动跳过。
- **会话 ID**：使用企业微信会话 ID（客服机器人为 `open_kfid + external_userid`，群聊机器人为群 ID），每个会话独立维护 CLI 状态。
- **自动重连**：WebSocket 断线时以指数退避（5s → 最大 60s）自动重连。
- **令牌刷新**：`access_token` 和 `aibot_ticket` 在到期前自动刷新。

## 常见问题

| 现象 | 可能原因 |
|------|---------|
| 报 `transport: wecom 需要 im_credentials.bot_id` | 未设置环境变量且 `im_credentials` 为空 |
| WS 连接报错 `40001` | `bot_secret` 错误 |
| WS 连接报错 `40013` | `bot_id` 无效，或机器人未开启长连接模式 |
| 消息收到但无回复 | 检查 profile 日志；同时确认管理后台已授予机器人回复权限 |
