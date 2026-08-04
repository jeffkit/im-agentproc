import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'IM-AgentProc',
  description: 'IM-side runtime for the agentproc ecosystem — bridge an IM transport to local coding CLIs via agentproc profiles',
  lang: 'en-US',

  // Published at https://jeffkit.github.io/im-agentproc/ (GitHub Pages project site).
  // Change this (or set a CNAME + '/') if a custom domain is bound later.
  base: '/im-agentproc/',

  locales: {
    root: {
      label: 'English',
      lang: 'en-US',
    },
    zh: {
      label: '中文',
      lang: 'zh-CN',
      themeConfig: {
        nav: [
          { text: '快速开始', link: '/zh/guide/quickstart' },
          { text: 'CLI 参考', link: '/zh/cli' },
          { text: 'Bridge', link: '/zh/bridge/' },
          { text: 'Transport', link: '/zh/transport' },
        ],
        sidebar: [
          {
            text: '简介',
            items: [
              { text: '什么是 IM-AgentProc？', link: '/zh/guide/what-is-im-agentproc' },
              { text: '快速开始', link: '/zh/guide/quickstart' },
              { text: '配置参考', link: '/zh/guide/configuration' },
            ],
          },
          {
            text: 'IM 接入指南',
            items: [
              { text: 'Telegram', link: '/zh/guide/telegram' },
              { text: '企业微信 (WeCom)', link: '/zh/guide/wecom' },
              { text: '飞书 (Feishu)', link: '/zh/guide/feishu' },
              { text: 'Discord', link: '/zh/guide/discord' },
              { text: '通过 MCP 出站投递', link: '/zh/guide/mcp-outbound' },
            ],
          },
          {
            text: '参考',
            items: [
              { text: 'CLI 参考', link: '/zh/cli' },
              { text: 'Bridge 运行模式', link: '/zh/bridge/' },
              { text: '内置 Profile 规范', link: '/zh/bridge/profile-spec' },
              { text: 'Transport 扩展', link: '/zh/transport' },
            ],
          },
          {
            text: 'AI Agent Skills',
            items: [
              { text: 'Skills 总览', link: '/zh/skills' },
            ],
          },
        ],
      },
    },
  },

  themeConfig: {
    siteTitle: 'IM-AgentProc',

    nav: [
      { text: 'Quick Start', link: '/guide/quickstart' },
      { text: 'CLI', link: '/cli' },
      { text: 'Bridge', link: '/bridge/' },
      { text: 'Transport', link: '/transport' },
    ],

    sidebar: {
      '/guide/': [
        {
          text: 'Introduction',
          items: [
            { text: 'What is IM-AgentProc?', link: '/guide/what-is-im-agentproc' },
            { text: 'Quick Start', link: '/guide/quickstart' },
            { text: 'Configuration', link: '/guide/configuration' },
          ],
        },
        {
          text: 'IM Platform Guides',
          items: [
            { text: 'Telegram', link: '/guide/telegram' },
            { text: 'WeCom', link: '/guide/wecom' },
            { text: 'Feishu (Lark)', link: '/guide/feishu' },
            { text: 'Discord', link: '/guide/discord' },
            { text: 'Outbound delivery via MCP', link: '/guide/mcp-outbound' },
          ],
        },
      ],
      '/cli/': [
        {
          text: 'Reference',
          items: [
            { text: 'CLI reference', link: '/cli' },
          ],
        },
      ],
      '/bridge/': [
        {
          text: 'Bridge',
          items: [
            { text: 'Run modes', link: '/bridge/' },
            { text: 'Built-in profile spec', link: '/bridge/profile-spec' },
          ],
        },
      ],
      '/transport/': [
        {
          text: 'Transport',
          items: [
            { text: 'Transport trait', link: '/transport' },
          ],
        },
      ],
    },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/jeffkit/im-agentproc' },
    ],

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © 2026 jeffkit',
    },

    search: {
      provider: 'local',
    },
  },
})
