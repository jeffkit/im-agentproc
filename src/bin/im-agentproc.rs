//! Connect to iLink Hub and run a local CLI for each inbound text message.
//!
//! - **显式 Token**：`--token` / `WEIXIN_TOKEN`
//! - **扫码配对**：`--pair`（或首次无凭证且你希望用手机确认）
//! - **零交互（默认）**：不传 token、且凭证路径**不存在**时，进程自行调用 Hub 的通用 `POST /hub/register`，
//!   将虚拟 token 写入本地 JSON（与配对成功后的格式相同），Hub 侧不区分调用方类型。
//!   若凭证文件**已存在但损坏或 token 为空**，默认**不会**静默覆盖（避免误伤扫码配对）；需删文件、
//!   用 `--token` / `--pair`，或显式 **`--force-register`**。
//!
//! 若 Hub 配置了 `ILINK_ADMIN_TOKEN`，本进程注册时需在同一环境中设置该变量。
//!
//! **调试**：`ILINKHUB_BRIDGE_DUMP_MSG=1`（或 `true` / `yes`）时在 stderr 打印每条入站的完整 `WeixinMessage` JSON 与各 `item_list[*].extra`。
//!
//! **内置 Profile**：`im-agentproc profile <type>` 运行内置 profile 处理器（如 `claude-code`），
//! 遵循 P0 exec 协议：从 `AGENT_*` 环境变量读取输入，向 stdout 写出回复。
//!
//! 配置见 `docs/bridge/index.md`，内置 profile 规范见 `docs/bridge/profile-spec.md`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use std::io::IsTerminal;

use anyhow::Context;
use im_agentproc::bridge::transport::{
    DiscordTransport, FeishuTransport, IlinkTransport, NullTransport, TelegramTransport, Transport,
    WecomTransport,
};
use im_agentproc::bridge::{
    builtin, default_direct_credential_path, default_local_credential_path,
    resolve_direct_connection, resolve_hub_connection, run_bridge_with_shutdown, BridgeApp,
    BridgeStop, Via,
};
use im_agentproc::mcp::{
    run_server, OutboundDelivery, SendFileTool, SendImageTool, SendTextTool, SendVoiceTool,
    ServerConfig, ToolRegistry,
};
use im_agentproc::paths::{
    default_bridge_config_path, default_bridge_manager_credentials_dir, default_bridge_profiles_dir,
};

#[derive(Parser)]
#[command(name = "im-agentproc")]
#[command(
    version,
    about = "将微信（通过 iLink Hub）桥接到本地编码 CLI (Claude Code, Codex, …) / Bridge WeChat (via iLink Hub) to a local coding CLI (Claude Code, Codex, …)"
)]
struct Cli {
    /// Hub base URL (same as WEIXIN_BASE_URL for other backends).
    #[arg(
        long,
        env = "WEIXIN_BASE_URL",
        default_value_t = get_hub_url_default(),
        global = true
    )]
    hub_url: String,

    /// Virtual token. Omit to use saved local credentials, auto-register, or `--pair` QR flow.
    #[arg(long, env = "WEIXIN_TOKEN", global = true)]
    token: Option<String>,

    /// Local credential JSON path (default: ~/.ilink-hub/bridge-credentials.json).
    #[arg(long, env = "ILINKHUB_BRIDGE_CREDS", global = true)]
    cred_file: Option<String>,

    /// Ignore saved credentials and run Hub QR pairing (phone confirm).
    #[arg(long, default_value_t = false, global = true)]
    pair: bool,

    /// Stable client name when auto-registering via `/hub/register`.
    /// Default: `local-<hostname>-<config-stem>` (e.g. `local-MacBook-ilink-claude`).
    #[arg(long, env = "ILINKHUB_BRIDGE_REGISTER_NAME", global = true)]
    register_name: Option<String>,

    /// If the credential file exists but is invalid or has an empty token, delete it and auto-register again.
    #[arg(long, default_value_t = false, global = true)]
    force_register: bool,

    /// Allow a non-`ilink` `transport:` to load its placeholder adapter. Without
    /// this flag a non-ilink transport fails fast at startup (it would otherwise
    /// back off forever as a zombie). Intended for pluggability smoke-tests only.
    #[arg(
        long,
        default_value_t = false,
        env = "ILINKHUB_BRIDGE_ALLOW_NULL_TRANSPORT",
        global = true
    )]
    allow_null_transport: bool,

    /// Disable interactive flows (QR login prompts). When set (or when stdout
    /// is not a TTY), `via: direct` bails instead of printing a QR code — a
    /// headless supervisor cannot confirm a phone scan. The bridge manager
    /// injects this env into its children so they fail fast and let the
    /// manager's credential guard park the profile.
    #[arg(
        long,
        default_value_t = false,
        env = "ILINKHUB_BRIDGE_NON_INTERACTIVE",
        global = true
    )]
    no_interactive: bool,

    /// Path to bridge YAML (command, args, timeout, …). Used only in bridge (default) mode.
    /// Defaults to `~/.ilink-hub/ilink-hub-bridge.yaml`.
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a built-in profile handler (P0 exec protocol: reads AGENT_* env vars, writes to stdout).
    ///
    /// Example: im-agentproc profile claude-code
    ///
    /// Built-in types:
    ///   claude-code   Wrap the `claude` CLI with automatic --resume session continuity
    Profile {
        /// Built-in profile type (e.g. `claude-code`).
        #[arg(value_name = "TYPE")]
        profile_type: String,
    },
    /// Run the MCP stdio server that exposes outbound delivery tools
    /// (`send_text` / `send_image` / `send_file` / `send_voice`) to a hub
    /// profile child process.
    ///
    /// The bridge manager launches this sub-process with a transport and
    /// inbound context already resolved; the sub-process reads
    /// `IM_AGENTPROC_MCP_*` env vars to discover what to serve.
    ///
    /// Example:
    ///   IM_AGENTPROC_MCP_TRANSPORT=feishu \
    ///   IM_AGENTPROC_MCP_CONTEXT_TOKEN=oc_xxx \
    ///   IM_AGENTPROC_MCP_TO_USER=user_1 \
    ///   im-agentproc mcp-server
    McpServer,
    /// Discover profile YAML files and supervise one bridge workspace per file.
    ///
    /// Each `*.yaml` / `*.yml` file keeps the existing bridge YAML format. The manager derives a
    /// stable workspace/register name from the file stem and stores a separate credential JSON per
    /// file, so every child bridge registers as an independent Hub backend.
    Manager {
        /// Directory containing bridge profile YAML files.
        #[arg(long, default_value_os_t = default_bridge_profiles_dir())]
        profiles_dir: PathBuf,

        /// Directory for per-profile bridge credential JSON files.
        #[arg(long, default_value_os_t = default_bridge_manager_credentials_dir())]
        credentials_dir: PathBuf,

        /// Seconds between profile directory scans.
        #[arg(long, default_value_t = 5)]
        scan_interval_secs: u64,

        /// Minimum seconds before restarting an exited child bridge.
        #[arg(long, default_value_t = 5)]
        restart_backoff_secs: u64,

        /// Maximum seconds for exponential child restart backoff.
        #[arg(long, default_value_t = 60)]
        max_restart_backoff_secs: u64,
    },
}

/// Resolves on SIGTERM on Unix; never resolves on other platforms.
/// Lets us use SIGTERM in `tokio::select!` without `#[cfg]` inside the macro.
async fn make_sigterm_future() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await;
}

fn explicit_token(cli: &Cli) -> Option<&str> {
    cli.token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The localhost Hub URL used as the CLI default when `WEIXIN_BASE_URL` is unset.
/// `via: direct` refusing to fall back to this value prevents silently pointing
/// a direct bridge at a Hub/localhost (review M2).
const DEFAULT_HUB_URL: &str = "http://127.0.0.1:8765";

/// Resolve the iLink upstream base URL for a `via: direct` profile (review M2).
///
/// - A YAML `base_url:` always wins (lets a manager mix hub/direct profiles
///   against different upstreams).
/// - Otherwise the CLI/env `--hub-url` / `WEIXIN_BASE_URL` is used — but the
///   localhost Hub default is rejected, so a direct bridge never silently
///   targets a Hub/localhost and fires `get_bot_qrcode` at the wrong server.
fn resolve_direct_base_url(direct_base_url: Option<&str>, cli_hub_url: &str) -> Result<String> {
    if let Some(b) = direct_base_url {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "via: direct profile has empty `base_url:`; set it to the real iLink upstream"
            );
        }
        return Ok(trimmed.trim_end_matches('/').to_string());
    }
    let cli_base = cli_hub_url.trim().trim_end_matches('/').to_string();
    if cli_base == DEFAULT_HUB_URL {
        anyhow::bail!(
            "via: direct 需要显式 `base_url:`（YAML）或非默认 `WEIXIN_BASE_URL` 指向真实 iLink 上游。\
             当前 base 仍是默认 localhost Hub 地址 ({DEFAULT_HUB_URL})，直接对它发 get_bot_qrcode \
             语义错误。若确实要连本机 Hub，请改用 `via: hub`。"
        );
    }
    Ok(cli_base)
}

/// Build the configured transport for the bridge run.
///
/// - `transport: ilink` (default): resolve credentials via Hub or direct iLink.
/// - `transport: telegram`: Telegram Bot API long-poll. Requires `im_credentials.token`.
/// - `transport: wecom`: WeCom smart-bot WebSocket. Requires `im_credentials.bot_id` + `bot_secret`.
/// - `transport: feishu`: Feishu WebSocket long connection. Requires `im_credentials.app_id` + `app_secret`.
/// - `transport: discord`: Discord Gateway WebSocket. Requires `im_credentials.token`.
/// - `transport: <other>`: load a `NullTransport` placeholder only when
///   `--allow-null-transport` is set; otherwise fail fast.
async fn run_mcp_server() -> Result<()> {
    use std::sync::Arc;

    // The bridge manager launches this sub-process after resolving the
    // transport + inbound context. We read those from env vars here.
    let transport_name = std::env::var("IM_AGENTPROC_MCP_TRANSPORT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "IM_AGENTPROC_MCP_TRANSPORT is required (one of ilink/telegram/wecom/feishu/discord)"
            )
        })?;
    let context_token = std::env::var("IM_AGENTPROC_MCP_CONTEXT_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("IM_AGENTPROC_MCP_CONTEXT_TOKEN is required"))?;
    let to_user = std::env::var("IM_AGENTPROC_MCP_TO_USER").unwrap_or_default();

    let transport = resolve_mcp_transport(&transport_name)?;

    let delivery = Arc::new(OutboundDelivery::new(transport, context_token, to_user));
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(SendTextTool {
        delivery: delivery.clone(),
    }));
    registry.register(Arc::new(SendImageTool {
        delivery: delivery.clone(),
    }));
    registry.register(Arc::new(SendFileTool {
        delivery: delivery.clone(),
    }));
    registry.register(Arc::new(SendVoiceTool { delivery }));

    let cfg = ServerConfig::new(Arc::new(registry));
    run_server(cfg).await
}

/// Build the [`Transport`] for the MCP sub-process from `IM_AGENTPROC_MCP_*`
/// env vars. Credentials are pulled from env so the manager doesn't have to
/// leak them through argv (which is visible in `ps(1)`).
///
/// Synchronous so unit tests can call it without spinning up a Tokio runtime.
fn resolve_mcp_transport(transport_name: &str) -> Result<Arc<dyn Transport>> {
    use std::sync::Arc;
    let env_var = |key: &str| -> Result<String> {
        std::env::var(key)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("{key} is required"))
    };
    let transport: Arc<dyn Transport> = match transport_name {
        "telegram" => Arc::new(TelegramTransport::new(env_var(
            "IM_AGENTPROC_MCP_TELEGRAM_TOKEN",
        )?)?),
        "feishu" => Arc::new(FeishuTransport::new(
            env_var("IM_AGENTPROC_MCP_FEISHU_APP_ID")?,
            env_var("IM_AGENTPROC_MCP_FEISHU_APP_SECRET")?,
        )?),
        "wecom" => Arc::new(WecomTransport::new(
            env_var("IM_AGENTPROC_MCP_WECOM_BOT_ID")?,
            env_var("IM_AGENTPROC_MCP_WECOM_BOT_SECRET")?,
        )),
        "discord" => Arc::new(DiscordTransport::new(env_var(
            "IM_AGENTPROC_MCP_DISCORD_TOKEN",
        )?)?),
        "ilink" => Arc::new(IlinkTransport::new(
            env_var("IM_AGENTPROC_MCP_ILINK_HUB_URL")?,
            env_var("IM_AGENTPROC_MCP_ILINK_TOKEN")?,
        )?),
        other => anyhow::bail!(
            "unknown transport `{other}` for mcp-server; expected ilink/telegram/wecom/feishu/discord"
        ),
    };
    Ok(transport)
}

async fn build_transport(
    app: &BridgeApp,
    cli: &Cli,
    config_path: &Path,
    description: Option<&str>,
    interactive: bool,
) -> Result<Arc<dyn Transport>> {
    let transport = app.transport();
    let creds = app.im_credentials();

    let t: Arc<dyn Transport> = match transport.as_str() {
        "ilink" => build_ilink_transport(app, cli, config_path, description, interactive).await?,

        "telegram" => {
            let token = creds
                .get("token")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("TELEGRAM_BOT_TOKEN")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context(
                    "transport: telegram 需要 im_credentials.token 或环境变量 TELEGRAM_BOT_TOKEN",
                )?;
            info!(
                transport = "telegram",
                "building Telegram Bot API transport"
            );
            Arc::new(TelegramTransport::new(token).context("build Telegram transport")?)
        }

        "wecom" => {
            let bot_id = creds
                .get("bot_id")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("WECOM_BOT_ID")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context("transport: wecom 需要 im_credentials.bot_id 或环境变量 WECOM_BOT_ID")?;
            let bot_secret = creds
                .get("bot_secret")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("WECOM_BOT_SECRET")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context(
                    "transport: wecom 需要 im_credentials.bot_secret 或环境变量 WECOM_BOT_SECRET",
                )?;
            info!(
                transport = "wecom",
                "building WeCom Bot WebSocket transport"
            );
            Arc::new(WecomTransport::new(bot_id, bot_secret))
        }

        "feishu" => {
            let app_id = creds
                .get("app_id")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("FEISHU_APP_ID")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context("transport: feishu 需要 im_credentials.app_id 或环境变量 FEISHU_APP_ID")?;
            let app_secret = creds
                .get("app_secret")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("FEISHU_APP_SECRET")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context(
                    "transport: feishu 需要 im_credentials.app_secret 或环境变量 FEISHU_APP_SECRET",
                )?;
            info!(transport = "feishu", "building Feishu WebSocket transport");
            Arc::new(FeishuTransport::new(app_id, app_secret).context("build Feishu transport")?)
        }

        "discord" => {
            let token = creds
                .get("token")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    std::env::var("DISCORD_BOT_TOKEN")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .context(
                    "transport: discord 需要 im_credentials.token 或环境变量 DISCORD_BOT_TOKEN",
                )?;
            info!(
                transport = "discord",
                "building Discord Gateway WebSocket transport"
            );
            Arc::new(DiscordTransport::new(token).context("build Discord transport")?)
        }

        name => {
            if !cli.allow_null_transport {
                anyhow::bail!(
                    "transport `{name}` 没有真实适配器（占位 NullTransport 会永久退避成僵尸进程）。\
                     如仅为可插拔冒烟测试，请加 `--allow-null-transport` 显式开启占位。"
                );
            }
            info!(transport = %name, "loading placeholder transport (allow_null_transport)");
            Arc::new(NullTransport::new(name.to_string()))
        }
    };

    let caps = t.capabilities();
    info!(
        transport = transport.as_str(),
        media_upload = caps.media_upload,
        "transport built"
    );
    Ok(t)
}

/// Build the iLink transport (Hub or Direct). Extracted from the main `build_transport`
/// to keep it readable.
async fn build_ilink_transport(
    app: &BridgeApp,
    cli: &Cli,
    config_path: &Path,
    description: Option<&str>,
    interactive: bool,
) -> Result<Arc<dyn Transport>> {
    let t: Arc<dyn Transport> = match app.via() {
        Via::Hub => {
            let (hub_url, token) = resolve_hub_connection(
                &cli.hub_url,
                explicit_token(cli),
                cli.cred_file.as_deref(),
                cli.pair,
                cli.register_name.as_deref(),
                cli.force_register,
                Some(config_path),
                description,
            )
            .await?;
            info!(%hub_url, via = "hub", "using Hub base URL for downstream");
            Arc::new(IlinkTransport::new(hub_url, token).context("build iLink transport")?)
        }
        Via::Direct => {
            let base = resolve_direct_base_url(app.direct_base_url(), &cli.hub_url)?;
            let (base, token) = resolve_direct_connection(
                &base,
                explicit_token(cli),
                cli.cred_file.as_deref(),
                cli.pair,
                cli.force_register,
                Some(config_path),
                interactive,
            )
            .await?;
            info!(base = %base, via = "direct", "connecting directly to iLink upstream");
            info!(
                "via: direct 不支持跨消息 CLI 会话续接（真实上游不回显 session_id）；每条消息起新 CLI 会话。"
            );
            Arc::new(IlinkTransport::new(base, token).context("build iLink transport (direct)")?)
        }
    };
    let caps = t.capabilities();
    info!(via = ?app.via(), media_upload = caps.media_upload, "iLink transport capabilities");
    Ok(t)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("im_agentproc=info".parse()?),
        )
        .init();

    let has_deprecated_addr = std::env::var("ILINK_HUB_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    let has_deprecated_url = std::env::var("ILINK_HUB_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    let has_new_url = std::env::var("WEIXIN_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    if (has_deprecated_addr || has_deprecated_url) && !has_new_url {
        tracing::warn!(
            "The environment variables `ILINK_HUB_ADDR` and `ILINK_HUB_URL` are deprecated. \
             Please migrate to `WEIXIN_BASE_URL`."
        );
    }

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Profile { profile_type }) => {
            // Run as a built-in profile subprocess (P0 exec protocol).
            // No Hub connection needed — just read env vars and write to stdout.
            builtin::run_builtin_profile(profile_type).await
        }
        Some(Commands::McpServer) => run_mcp_server().await,
        Some(Commands::Manager {
            profiles_dir,
            credentials_dir,
            scan_interval_secs,
            restart_backoff_secs,
            max_restart_backoff_secs,
        }) => {
            if explicit_token(&cli).is_some()
                || cli.cred_file.is_some()
                || cli.register_name.is_some()
                || cli.pair
            {
                tracing::warn!(
                    "manager mode ignores --token/WEIXIN_TOKEN, --cred-file, --register-name, and --pair; \
                     each profile gets an independent auto-registered child bridge"
                );
            }
            // Child bridges inherit this process's environment, so a manager-level
            // ILINK_ADMIN_TOKEN propagates to every child's `/hub/register` call. If it is
            // missing and the Hub enforces admin auth, auto-registration fails with 401 and
            // operators are tempted to hand-craft credentials that reuse another backend's
            // vtoken — which makes multiple bridges share one message queue (split-brain).
            if std::env::var("ILINK_ADMIN_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .is_none()
            {
                tracing::warn!(
                    "ILINK_ADMIN_TOKEN is not set for the bridge manager. If the Hub enforces \
                     admin auth, child bridges will fail to auto-register (HTTP 401). Set \
                     ILINK_ADMIN_TOKEN (matching the Hub) in the manager's environment so each \
                     profile registers as an independent backend. Never reuse another backend's \
                     credentials/token to work around this — sharing a vtoken makes bridges \
                     compete for the same message queue."
                );
            }
            let mut opts = im_agentproc::bridge::manager::BridgeManagerOptions::new(
                cli.hub_url.clone(),
                profiles_dir.clone(),
                credentials_dir.clone(),
            );
            opts.scan_interval = std::time::Duration::from_secs((*scan_interval_secs).max(1));
            opts.restart_backoff = std::time::Duration::from_secs((*restart_backoff_secs).max(1));
            opts.max_restart_backoff =
                std::time::Duration::from_secs((*max_restart_backoff_secs).max(1));
            opts.force_register = cli.force_register;
            im_agentproc::bridge::manager::run_bridge_manager(opts).await
        }
        None => {
            // Default mode: connect to Hub and long-poll for messages.
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(default_bridge_config_path);
            let app = BridgeApp::load(&config_path)?;
            info!(config_path = %config_path.display(), "loaded bridge config");

            // Startup probe to verify that the CLI command(s) exist and are usable.
            for name in app.profile_names() {
                if let Some(profile) = app.profile(name) {
                    if let Err(e) = im_agentproc::bridge::probe_profile_light(profile) {
                        eprintln!("Startup probe failed for profile `{}`: {}", name, e);
                        std::process::exit(1);
                    }
                }
            }

            let cred_path = cli
                .cred_file
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| match app.via() {
                    Via::Direct => default_direct_credential_path(),
                    Via::Hub => default_local_credential_path(),
                });
            let using_explicit_token = explicit_token(&cli).is_some();

            // Interactive flows (QR login) require a TTY for stdout and must not
            // be disabled via --no-interactive / ILINKHUB_BRIDGE_NON_INTERACTIVE
            // (the manager injects the latter so its children fail fast instead
            // of QR-blocking headless — review N1).
            let interactive = !cli.no_interactive && std::io::stdout().is_terminal();

            // Shared shutdown token — cancelled by Ctrl-C or SIGTERM so that
            // in-flight AI calls are gracefully cancelled and users are notified.
            let shutdown = tokio_util::sync::CancellationToken::new();

            // Build a SIGTERM future once, outside the reconnect loop.
            // On non-Unix platforms this never resolves (pending forever).
            let sigterm_fut = make_sigterm_future();
            tokio::pin!(sigterm_fut);

            'reconnect: loop {
                // Get description from default profile for registration
                let description = app
                    .profile(app.default_profile_name())
                    .and_then(|p| p.description.as_deref());

                let transport =
                    build_transport(&app, &cli, &config_path, description, interactive).await?;

                let mut handle = tokio::spawn(run_bridge_with_shutdown(
                    transport,
                    app.clone(),
                    shutdown.clone(),
                ));

                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("bridge received Ctrl-C; shutting down gracefully");
                        shutdown.cancel();
                        // Wait up to 3 s for error replies to be sent before aborting.
                        // Only abort+await when the task did NOT finish within the timeout;
                        // re-awaiting an already-completed JoinHandle causes a panic.
                        let timed_out = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            &mut handle,
                        ).await.is_err();
                        if timed_out {
                            handle.abort();
                            let _ = handle.await;
                        }
                        info!("exit");
                        return Ok(());
                    }
                    _ = &mut sigterm_fut => {
                        info!("bridge received SIGTERM; shutting down gracefully");
                        shutdown.cancel();
                        let timed_out = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            &mut handle,
                        ).await.is_err();
                        if timed_out {
                            handle.abort();
                            let _ = handle.await;
                        }
                        return Ok(());
                    }
                    result = &mut handle => {
                        match result {
                            Ok(BridgeStop::TokenRejected) if using_explicit_token => {
                                let via = app.via();
                                let hint = if via.is_direct() {
                    "via: direct 下请重新 `--pair` 扫码登录真实上游，或更换为有效的 WEIXIN_TOKEN。"
                                } else {
                    "via: hub 下请重新执行 `ilink-hub register` 或 `im-agentproc --force-register`。"
                                };
                                anyhow::bail!(
                                    "WEIXIN_TOKEN / --token 被拒绝（未注册或已失效）。{hint}"
                                );
                            }
                            Ok(BridgeStop::TokenRejected) => {
                                let via = app.via();
                                let what = if via.is_direct() {
                                    "direct token"
                                } else {
                                    "hub token"
                                };
                                warn!(
                                    path = %cred_path.display(),
                                    "{what} revoked at runtime; removing credentials and reconnecting"
                                );
                                let _ = tokio::fs::remove_file(&cred_path).await;
                                continue 'reconnect;
                            }
                            Ok(BridgeStop::FatalCliError(reason)) => {
                                anyhow::bail!(
                                    "CLI 认证失败，需要用户处理后重启 bridge：{reason}"
                                );
                            }
                            Ok(BridgeStop::Shutdown) => {
                                info!("bridge shut down gracefully");
                                return Ok(());
                            }
                            Err(e) => {
                                return Err(e).context("bridge task panicked or failed");
                            }
                        }
                    }
                }
            }
        }
    }
}

fn get_hub_url_default() -> String {
    if let Ok(val) = std::env::var("WEIXIN_BASE_URL") {
        if !val.trim().is_empty() {
            return val.trim().to_string();
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_URL") {
        if !val.trim().is_empty() {
            return val.trim().to_string();
        }
    }
    if let Ok(val) = std::env::var("ILINK_HUB_ADDR") {
        if !val.trim().is_empty() {
            let val_trimmed = val.trim();
            if val_trimmed.starts_with("http://") || val_trimmed.starts_with("https://") {
                return val_trimmed.to_string();
            } else {
                return format!("http://{}", val_trimmed);
            }
        }
    }
    "http://127.0.0.1:8765".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_yaml(dir: &Path, content: &str) -> PathBuf {
        let p = dir.join("profile.yaml");
        std::fs::write(&p, content).unwrap();
        p
    }

    /// Global mutex serialising any test that mutates process env. Tests run
    /// in parallel by default, but `std::env::set_var` / `remove_var` mutate
    /// shared global state and would race otherwise.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Clear IM-credential env vars that build_transport falls back to.
    /// Held under `ENV_LOCK` so no other test can re-set them mid-flight.
    fn clear_im_credential_env() {
        // SAFETY: caller holds ENV_LOCK.
        unsafe {
            std::env::remove_var("TELEGRAM_BOT_TOKEN");
            std::env::remove_var("DISCORD_BOT_TOKEN");
            std::env::remove_var("WECOM_BOT_ID");
            std::env::remove_var("WECOM_BOT_SECRET");
            std::env::remove_var("FEISHU_APP_ID");
            std::env::remove_var("FEISHU_APP_SECRET");
        }
    }

    #[test]
    fn resolve_direct_base_url_rejects_default_hub_url_without_base_url() {
        // M2: via: direct, no `base_url:`, CLI hub-url at the localhost default → bail.
        let err = resolve_direct_base_url(None, "http://127.0.0.1:8765").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct") && msg.contains("base_url"),
            "expected M2 bail mentioning via: direct + base_url: {msg}"
        );
    }

    #[test]
    fn resolve_direct_base_url_accepts_non_default_cli_url() {
        // A non-default --hub-url / WEIXIN_BASE_URL is acceptable for direct.
        let base = resolve_direct_base_url(None, "https://ilinkai.weixin.qq.com/").unwrap();
        assert_eq!(base, "https://ilinkai.weixin.qq.com");
    }

    #[test]
    fn resolve_direct_base_url_yaml_overrides_cli() {
        // `base_url:` in YAML wins even if the CLI url is the localhost default.
        let base = resolve_direct_base_url(
            Some("https://ilinkai.weixin.qq.com"),
            "http://127.0.0.1:8765",
        )
        .unwrap();
        assert_eq!(base, "https://ilinkai.weixin.qq.com");
    }

    #[test]
    fn resolve_direct_base_url_rejects_empty_yaml_base_url() {
        let err = resolve_direct_base_url(Some("   "), "https://up.example.com").unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn no_interactive_bare_flag_parses_true() {
        // N1 regression guard: `--no-interactive` must be a bare SetTrue flag
        // (the manager passes it bare to children). The env form only accepts
        // "true"/"false", so the manager must NOT inject "1" via env.
        let cli = Cli::parse_from(["im-agentproc", "--no-interactive", "--hub-url", "http://x"]);
        assert!(cli.no_interactive);
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://x"]);
        assert!(!cli.no_interactive);
    }

    #[test]
    fn build_transport_direct_bails_without_base_url() {
        // End-to-end-ish: build_transport on a via: direct profile with no base_url
        // and the default hub-url bails at the M2 gate before any network call.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\nvia: direct\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected M2 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct") && msg.contains("base_url"),
            "expected M2 bail: {msg}"
        );
    }

    #[test]
    fn build_transport_unknown_transport_bails_without_allow_flag() {
        // L4: an unknown transport fails fast unless --allow-null-transport is set.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: foobar-unknown\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected L4 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foobar-unknown") && msg.contains("--allow-null-transport"),
            "expected L4 bail mentioning transport + flag: {msg}"
        );
    }

    #[test]
    fn build_transport_wecom_bails_without_credentials() {
        // wecom is a real transport; without credentials it should bail fast.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: wecom\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected credential bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("bot_id") || msg.to_lowercase().contains("wecom"),
            "expected missing credentials error: {msg}"
        );
    }

    #[test]
    fn build_transport_telegram_bails_without_token() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: telegram\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected credential bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("token") && msg.to_lowercase().contains("telegram"),
            "expected telegram/token error: {msg}"
        );
    }

    #[test]
    fn build_transport_feishu_bails_without_app_secret() {
        // Only app_id is set; the second secret check should fire.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\n\
             transport: feishu\n\
             im_credentials:\n  app_id: \"cli_xxx\"\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected credential bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("app_secret") && msg.contains("feishu"),
            "expected feishu/app_secret error: {msg}"
        );
    }

    #[test]
    fn build_transport_wecom_bails_without_bot_secret() {
        // bot_id is set but bot_secret isn't — the second secret check should fire.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\n\
             transport: wecom\n\
             im_credentials:\n  bot_id: \"wxyz\"\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected credential bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bot_secret") && msg.contains("wecom"),
            "expected wecom/bot_secret error: {msg}"
        );
    }

    #[test]
    fn build_transport_discord_bails_without_token() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: discord\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match rt.block_on(build_transport(&app, &cli, &cfg, None, true)) {
            Ok(_) => panic!("expected credential bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("token") && msg.to_lowercase().contains("discord"),
            "expected discord/token error: {msg}"
        );
    }

    // ── resolve_mcp_transport env-var wiring ─────────────────────────────────
    // We serialise on the same ENV_LOCK + clear_im_credential_env helper used
    // by the factory tests so concurrent runs can't race on env reads.
    #[test]
    fn resolve_mcp_transport_unknown_name_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        match resolve_mcp_transport("webex") {
            Err(err) => assert!(format!("{err:#}").contains("unknown transport")),
            Ok(_) => panic!("expected unknown-transport error"),
        }
    }

    #[test]
    fn resolve_mcp_transport_telegram_missing_token_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        match resolve_mcp_transport("telegram") {
            Err(err) => assert!(format!("{err:#}").contains("TELEGRAM_TOKEN")),
            Ok(_) => panic!("expected missing-credential error"),
        }
    }

    #[test]
    fn resolve_mcp_transport_feishu_missing_app_secret_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        // SAFETY: held under ENV_LOCK.
        unsafe {
            std::env::set_var("IM_AGENTPROC_MCP_FEISHU_APP_ID", "cli_x");
        }
        let result = resolve_mcp_transport("feishu");
        unsafe {
            std::env::remove_var("IM_AGENTPROC_MCP_FEISHU_APP_ID");
        }
        match result {
            Err(err) => assert!(format!("{err:#}").contains("FEISHU_APP_SECRET")),
            Ok(_) => panic!("expected missing-credential error"),
        }
    }

    #[tokio::test]
    async fn resolve_mcp_transport_discord_succeeds_when_token_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        // SAFETY: held under ENV_LOCK.
        unsafe {
            std::env::set_var("IM_AGENTPROC_MCP_DISCORD_TOKEN", "test-discord");
        }
        // DiscordTransport::new spawns a WS worker that will fail to
        // connect to the real Gateway — that's fine; we only need to verify
        // the env-var → Transport construction chain works.
        let result = resolve_mcp_transport("discord");
        unsafe {
            std::env::remove_var("IM_AGENTPROC_MCP_DISCORD_TOKEN");
        }
        let transport = result.expect("DiscordTransport should build from env");
        assert_eq!(transport.name(), "discord");
        assert!(transport.capabilities().media_upload);
    }

    #[test]
    fn build_transport_unknown_transport_succeeds_with_allow_flag() {
        // With --allow-null-transport, an unknown transport loads a NullTransport
        // placeholder instead of failing. Confirms the escape hatch actually works.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: foobar-unknown\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from([
            "im-agentproc",
            "--hub-url",
            "http://127.0.0.1:8765",
            "--allow-null-transport",
        ]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let t = rt
            .block_on(build_transport(&app, &cli, &cfg, None, true))
            .expect("NullTransport placeholder should build");
        // NullTransport advertises default capabilities (no media_upload).
        let caps = t.capabilities();
        assert!(!caps.media_upload);
    }

    #[test]
    fn build_transport_telegram_falls_back_to_env_token() {
        // No im_credentials.token in YAML, but TELEGRAM_BOT_TOKEN in env → must
        // construct successfully. TelegramTransport::new is purely local
        // (no network), so this stays hermetic.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: telegram\n",
        );
        let app = BridgeApp::load(&cfg).unwrap();
        let cli = Cli::parse_from(["im-agentproc", "--hub-url", "http://127.0.0.1:8765"]);
        // SAFETY: held under ENV_LOCK.
        unsafe {
            std::env::set_var("TELEGRAM_BOT_TOKEN", "test-token-only");
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(build_transport(&app, &cli, &cfg, None, true));
        // SAFETY: still under ENV_LOCK; _guard drops at end of fn.
        unsafe {
            std::env::remove_var("TELEGRAM_BOT_TOKEN");
        }
        let t = result.expect("TelegramTransport should build from env var");
        let caps = t.capabilities();
        assert!(
            caps.media_upload,
            "Telegram transport reports media_upload=true (send_media implemented)"
        );
    }
}
