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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;


use im_agentproc::bridge::transport::registry::{TransportBuildCtx, TransportRegistry};
use im_agentproc::bridge::transport::Transport;
use im_agentproc::bridge::{
    builtin, BridgeApp, Via,
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

fn explicit_token(cli: &Cli) -> Option<&str> {
    cli.token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

use im_agentproc::bridge::transport::registry::DEFAULT_HUB_URL;


/// Launch the outbound-media MCP stdio server as a sub-process entrypoint.
///
/// The bridge manager launches this sub-process after resolving the
/// transport + inbound context. We read those from env vars here.
async fn run_mcp_server() -> Result<()> {
    use std::sync::Arc;

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

    let transport = resolve_mcp_transport(&transport_name).await?;

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
/// Credential keys follow the uniform `IM_AGENTPROC_MCP_{KIND}_{KEY}` scheme
/// (`{KEY}` lowercased into `im_credentials`, e.g. `IM_AGENTPROC_MCP_TELEGRAM_TOKEN`
/// → `token`); construction goes through the MCP registry
/// ([`TransportRegistry::with_mcp_builtins`]), so custom kinds registered
/// downstream resolve here too.
///
/// Async (factories are async); unit tests drive it via `#[tokio::test]`.
async fn resolve_mcp_transport(transport_name: &str) -> Result<Arc<dyn Transport>> {
    use std::collections::HashMap;

    let prefix = format!("IM_AGENTPROC_MCP_{}_", transport_name.to_ascii_uppercase());
    let creds: HashMap<String, String> = std::env::vars()
        .filter(|(k, v)| k.starts_with(&prefix) && !v.trim().is_empty())
        .map(|(k, v)| (k[prefix.len()..].to_ascii_lowercase(), v))
        .collect();

    let registry = TransportRegistry::with_mcp_builtins();
    if registry.get(transport_name).is_none() {
        anyhow::bail!(
            "unknown transport `{transport_name}` for mcp-server; expected {}",
            registry.kinds().join("/")
        );
    }
    let ctx = TransportBuildCtx {
        kind: transport_name.to_string(),
        via: Via::Hub,
        hub_url: String::new(),
        direct_base_url: None,
        im_credentials: creds,
        explicit_token: None,
        cred_file: None,
        force_pair: false,
        force_register: false,
        register_name: None,
        config_path: None,
        description: None,
        interactive: false,
        allow_null_placeholder: false,
    };
    registry.build(&ctx).await
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
            // Transport selection goes through the (overridable) registry; a
            // downstream crate can ship its own thin main registering custom
            // factories and delegating to bridge::run_loop — see docs/transport.md.
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(default_bridge_config_path);
            let app = BridgeApp::load(&config_path)?;
            info!(config_path = %config_path.display(), "loaded bridge config");

            im_agentproc::bridge::run_loop::run_bridge_reconnecting(
                im_agentproc::bridge::run_loop::BridgeRunOptions {
                    app,
                    config_path,
                    registry: TransportRegistry::with_builtins(),
                    hub_url: cli.hub_url.clone(),
                    explicit_token: explicit_token(&cli).map(str::to_string),
                    cred_file: cli.cred_file.clone(),
                    force_pair: cli.pair,
                    force_register: cli.force_register,
                    register_name: cli.register_name.clone(),
                    allow_null_transport: cli.allow_null_transport,
                    no_interactive: cli.no_interactive,
                },
            )
            .await
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
    DEFAULT_HUB_URL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use im_agentproc::bridge::transport::registry::resolve_direct_base_url;

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








    // ── resolve_mcp_transport env-var wiring ─────────────────────────────────
    // We serialise on the same ENV_LOCK + clear_im_credential_env helper used
    // by the factory tests so concurrent runs can't race on env reads.
    // Deliberately holds ENV_LOCK across awaits: the resolver reads process
    // env, and #[tokio::test] runs on a single-threaded runtime, so no other
    // task can observe the guarded window anyway.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_mcp_transport_unknown_name_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        match resolve_mcp_transport("webex").await {
            Err(err) => assert!(format!("{err:#}").contains("unknown transport")),
            Ok(_) => panic!("expected unknown-transport error"),
        }
    }

    // Deliberately holds ENV_LOCK across awaits: the resolver reads process
    // env, and #[tokio::test] runs on a single-threaded runtime, so no other
    // task can observe the guarded window anyway.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_mcp_transport_telegram_missing_token_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        match resolve_mcp_transport("telegram").await {
            // adapter-owned credential error names both sources
            Err(err) => assert!(
                format!("{err:#}").to_lowercase().contains("token"),
                "expected missing-credential error"
            ),
            Ok(_) => panic!("expected missing-credential error"),
        }
    }

    // Deliberately holds ENV_LOCK across awaits: the resolver reads process
    // env, and #[tokio::test] runs on a single-threaded runtime, so no other
    // task can observe the guarded window anyway.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_mcp_transport_feishu_missing_app_secret_bails() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_im_credential_env();
        // SAFETY: held under ENV_LOCK.
        unsafe {
            std::env::set_var("IM_AGENTPROC_MCP_FEISHU_APP_ID", "cli_x");
        }
        let result = resolve_mcp_transport("feishu").await;
        unsafe {
            std::env::remove_var("IM_AGENTPROC_MCP_FEISHU_APP_ID");
        }
        match result {
            Err(err) => assert!(
                format!("{err:#}").to_lowercase().contains("secret"),
                "expected missing-credential error"
            ),
            Ok(_) => panic!("expected missing-credential error"),
        }
    }

    // Deliberately holds ENV_LOCK across awaits: the resolver reads process
    // env, and #[tokio::test] runs on a single-threaded runtime, so no other
    // task can observe the guarded window anyway.
    #[allow(clippy::await_holding_lock)]
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
        let result = resolve_mcp_transport("discord").await;
        unsafe {
            std::env::remove_var("IM_AGENTPROC_MCP_DISCORD_TOKEN");
        }
        let transport = result.expect("DiscordTransport should build from env");
        assert_eq!(transport.name(), "discord");
        assert!(transport.capabilities().media_upload);
    }


}
