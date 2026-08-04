# AI Agent Skills

`im-agentproc` 提供可供 AI Coding Agent（Claude Code、Cursor、Codex 等）使用的 **Agent Skill**，让这些工具在项目内工作时能复用标准化工作流，而不必重新学习每个 CLI 的用法。

## agentproc-profile Skill

**功能**：创建、编辑、测试、发布 im-agentproc agentproc profile YAML；或用 agentproc SDK（Python / Node.js）开发自定义 handler。

**触发词**：「创建 agentproc profile」「新增 profile」「添加 profile」「配置 im-agentproc」「发布 profile」「接 claude-code / cursor」……

**Skill 文件位置**：

```
docs/public/skills/agentproc-profile/SKILL.md
```

**已发布 URL**（安装用）：

```
https://jeffkit.github.io/im-agentproc/skills/agentproc-profile/SKILL.md
```

### 在 Claude Code / Cursor 中安装

```bash
# Claude Code
claude skill add https://jeffkit.github.io/im-agentproc/skills/agentproc-profile/SKILL.md

# 或手动下载到用户 skills 目录
mkdir -p ~/.claude/skills/agentproc-profile
curl -L https://jeffkit.github.io/im-agentproc/skills/agentproc-profile/SKILL.md \
     -o ~/.claude/skills/agentproc-profile/SKILL.md
```

### Skill 能帮你做什么

1. **需求确认**：明确 profile 名称、使用场景（claude-code / 自定义脚本 / 其他 CLI）、工作目录
2. **生成 YAML**：正确写出 `agentproc:` 块结构，包含 executor / command / env / timeout 等字段
3. **SDK Handler**：提供 Python / Node.js 的 agentproc SDK 模板（含多轮对话、流式输出）
4. **测试命令**：给出 `echo "$TURN" | im-agentproc profile ...` 本地模拟调用
5. **发布与验证**：确认发布到 `~/.ilink-hub-bridge/profiles/` 并检查 manager 日志
6. **旧格式迁移**：从 0.3 格式（`profiles:` + `routing:`）自动迁移到 0.4 格式

### Skill 覆盖的内置 Executor

| executor | 说明 |
|----------|------|
| `claude-code` | Claude Code CLI，自动 `--resume` 续接 |
| `cursor` | Cursor background agent |
| `codebuddy` | 腾讯 CodeBuddy CLI |
| `codex` | OpenAI Codex CLI |
| `agy` | Google Antigravity CLI |
| `recursive` | Recursive agent CLI |
| `opencode` | OpenCode CLI |

---

## 其他资源

- **[配置参考](/zh/guide/configuration)**：所有 YAML 字段的完整文档
- **[内置 Profile 规范](/zh/bridge/profile-spec)**：agentproc 0.4 协议（turn 输入 / 事件输出）
- **[快速开始](/zh/guide/quickstart)**：安装与第一条消息
