# 飞书接入

本指南介绍如何使用 `feishu` transport 将飞书自建应用接入 agentproc profile。

## 工作原理

`feishu` transport 使用：
- **WebSocket 长连接**（通过 `larksuite-oapi-sdk-rs`）接收 `im.message.receive_v1` 事件，无需公网 URL
- **飞书 IM HTTP API**（`POST /open-apis/im/v1/messages`）发送回复

> SDK 从本地主动连接飞书 WebSocket 网关，不需要外网暴露接口。

## 前提条件

- 拥有管理员权限的飞书账号
- 已安装 `im-agentproc`

## 1. 创建自建应用

1. 进入[飞书开放平台](https://open.feishu.cn/app)
2. 点击 **创建企业自建应用**，填写名称和描述
3. 进入 **凭证与基础信息** 记录：
   - **App ID**（`app_id`）
   - **App Secret**（`app_secret`）

## 2. 配置事件订阅

1. 在应用控制台进入 **事件订阅**
2. 将 **请求地址配置方式** 设为 **使用长连接接收事件**（即 WebSocket 模式，无需公网 URL）
3. 订阅事件：`im.message.receive_v1`

## 3. 添加权限（Scope）

在 **权限管理** 中添加：
- `im:message` — 读取消息
- `im:message:send_as_bot` — 以机器人身份发送消息
- `im:message.file:download` — 下载消息中的文件/图片（媒体支持必须）

添加后发布应用版本以使权限生效。

## 4. 安装应用

在 **版本管理与发布** 中发布应用（或提交审核），然后在 **应用功能 → 机器人** 中将机器人安装到工作空间。

## 5. 编写 Profile YAML

```yaml
description: 我的飞书 AI 编程助手
transport: feishu
im_credentials:
  app_id: ${FEISHU_APP_ID}
  app_secret: ${FEISHU_APP_SECRET}
agentproc:
  executor: claude-code
```

设置环境变量：

```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxx
export FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

## 6. 启动 Bridge

```bash
FEISHU_APP_ID=cli_xx... \
FEISHU_APP_SECRET=xx... \
  im-agentproc --config ~/.im-agentproc/feishu-profile.yaml
```

Bridge 连接飞书 WebSocket 网关，开始接收消息。向机器人发送飞书消息，Bridge 处理后在同一会话中回复。

## 注意事项

- **支持的消息类型**：`text`（文本）、`image`（图片）、`file`（文件）、`audio`（音频）。其他类型静默丢弃。
- **媒体处理**：图片、文件和音频通过 `GET /open-apis/im/v1/messages/{id}/resources/{key}?type=image|file` 下载到本地临时文件，以 `file://` URL 形式作为 `MediaRef` 传递给 Agent。纯媒体消息（无文本）会生成 `[image]` 等占位文本以确保 Agent 收到消息。
- **防循环**：`sender_type: bot` 的消息自动跳过。
- **会话 ID** 是飞书的 `chat_id`，每个群聊/单聊独立维护 CLI 会话状态。
- **自动重连**：SDK 内置 `auto_reconnect=true`，自动处理重连。
- **令牌管理**：`tenant_access_token` 在到期前 5 分钟自动刷新，事件处理器和发送路径共享同一缓存。

## 常见问题

| 现象 | 可能原因 |
|------|---------|
| 报 `transport: feishu 需要 im_credentials.app_id` | 未设置环境变量且 `im_credentials` 为空 |
| SDK 客户端构建失败 | `app_id` 或 `app_secret` 格式有误 |
| 收到消息但无回复 | 检查 `im:message:send_as_bot` 权限是否已授予且应用已安装 |
| `tenant_access_token failed (code=10003)` | App Secret 错误 |
| 未收到事件 | 确认事件订阅中已选 "长连接接收事件" 且订阅了 `im.message.receive_v1` |
| 图片/文件下载失败（403） | 在应用权限中添加 `im:message.file:download` 并重新发布 |
