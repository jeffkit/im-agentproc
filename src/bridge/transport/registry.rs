//! Pluggable transport registry: kind string → async factory.
//!
//! New IM channels (飞书 / Telegram / …) register a factory under their
//! `transport:` kind instead of editing a hardcoded `match` in the binary.
//! The built-in registry wires up the in-tree adapters (`ilink`, `telegram`,
//! `wecom`, `feishu`, `discord` and the `null` placeholder); downstream crates
//! can build their own [`TransportRegistry`] and `register` additional adapters.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use tracing::info;

use super::connection::{resolve_direct_connection, resolve_hub_connection};
use super::discord::DiscordTransport;
use super::feishu::FeishuTransport;
use super::ilink::IlinkTransport;
use super::telegram::TelegramTransport;
use super::wecom::WecomTransport;
use super::{NullTransport, Transport};
use crate::bridge::config::Via;

/// The localhost Hub URL used as the CLI default when `WEIXIN_BASE_URL` is unset.
/// `via: direct` refusing to fall back to this value prevents silently pointing
/// a direct bridge at a Hub/localhost (review M2).
pub const DEFAULT_HUB_URL: &str = "http://127.0.0.1:8765";

/// Everything a transport factory may need to construct its adapter. Built from
/// the bridge profile (`BridgeApp`) plus CLI flags by the binary; factories must
/// not reach back into CLI types so the registry stays usable as a library.
pub struct TransportBuildCtx {
    /// `transport:` kind from the profile (the registry lookup key).
    pub kind: String,
    /// Credential resolution / connection target (`via:` field).
    pub via: Via,
    /// CLI/env Hub base URL (`--hub-url` / `WEIXIN_BASE_URL`).
    pub hub_url: String,
    /// Profile `base_url:` for `via: direct` (overrides `hub_url`).
    pub direct_base_url: Option<String>,
    /// Profile `im_credentials:` map (already `${VAR}`-expanded). Each adapter
    /// owns the parsing of its own keys — see `<Adapter>::from_credentials`.
    pub im_credentials: HashMap<String, String>,
    /// `--token` / `WEIXIN_TOKEN`, trimmed and empty-filtered.
    pub explicit_token: Option<String>,
    /// `--cred-file` override for the saved credential JSON.
    pub cred_file: Option<String>,
    /// `--pair`: force a fresh QR pairing even if credentials exist.
    pub force_pair: bool,
    /// `--force-register`: discard unusable saved credentials and re-register.
    pub force_register: bool,
    /// `--register-name`: client name for automatic Hub registration.
    pub register_name: Option<String>,
    /// Profile YAML path (stored alongside resolved credentials).
    pub config_path: Option<PathBuf>,
    /// Agent description recorded at registration time.
    pub description: Option<String>,
    /// Whether interactive flows (QR login on stdout TTY) are allowed.
    pub interactive: bool,
    /// Accept a `NullTransport` placeholder for kinds with no registered
    /// factory (CLI `--allow-null-transport`).
    pub allow_null_placeholder: bool,
}

/// Async factory building a transport for the given context.
pub type TransportFactory =
    Arc<dyn for<'a> Fn(&'a TransportBuildCtx) -> BoxFuture<'a, Result<Arc<dyn Transport>>> + Send + Sync>;

/// Registry mapping `transport:` kind strings to factories.
#[derive(Default)]
pub struct TransportRegistry {
    factories: HashMap<String, TransportFactory>,
}

impl TransportRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Registry preloaded with the built-in adapters: `ilink` (hub/direct
    /// credential flows), the four IM adapters and the `null` placeholder.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register("ilink", ilink_entry);
        reg.register("telegram", telegram_entry);
        reg.register("wecom", wecom_entry);
        reg.register("feishu", feishu_entry);
        reg.register("discord", discord_entry);
        reg.register("null", null_entry);
        reg
    }

    /// Registry for the MCP outbound subprocess (`im-agentproc mcp-server`).
    /// Unlike the profile path, there is no interactive credential flow: the
    /// manager resolves credentials into `IM_AGENTPROC_MCP_{KIND}_{KEY}` env
    /// vars, which the caller collects into `ctx.im_credentials`
    /// (`{key}` lowercased, e.g. `TELEGRAM_TOKEN` → `token`); factories then
    /// construct adapters directly.
    pub fn with_mcp_builtins() -> Self {
        let mut reg = Self::new();
        reg.register("ilink", mcp_ilink_entry);
        reg.register("telegram", telegram_entry);
        reg.register("wecom", wecom_entry);
        reg.register("feishu", feishu_entry);
        reg.register("discord", discord_entry);
        reg
    }

    /// Register (or replace) the factory for a transport kind.
    pub fn register<F>(&mut self, kind: &str, factory: F)
    where
        F: for<'a> Fn(&'a TransportBuildCtx) -> BoxFuture<'a, Result<Arc<dyn Transport>>>
            + Send
            + Sync
            + 'static,
    {
        self.factories.insert(kind.to_string(), Arc::new(factory));
    }

    pub fn get(&self, kind: &str) -> Option<&TransportFactory> {
        self.factories.get(kind)
    }

    pub fn kinds(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.factories.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Build the transport for `ctx`: run the registered factory, or fall back
    /// to the `NullTransport` placeholder when explicitly allowed (review L4 —
    /// an unknown kind without the flag fails fast instead of zombie-backoff).
    pub async fn build(&self, ctx: &TransportBuildCtx) -> Result<Arc<dyn Transport>> {
        if let Some(factory) = self.factories.get(&ctx.kind) {
            return factory(ctx).await;
        }
        if ctx.allow_null_placeholder {
            info!(transport = %ctx.kind, "loading placeholder transport (allow_null_transport)");
            return Ok(Arc::new(NullTransport::new(ctx.kind.clone())));
        }
        bail!(
            "transport `{}` 没有真实适配器（占位 NullTransport 会永久退避成僵尸进程）。\
             如仅为可插拔冒烟测试，请加 `--allow-null-transport` 显式开启占位。",
            ctx.kind
        );
    }
}

macro_rules! adapter_entry {
    ($entry:ident, $factory:ident, $adapter:ty, $log_name:literal, $log_msg:literal) => {
        fn $entry(ctx: &TransportBuildCtx) -> BoxFuture<'_, Result<Arc<dyn Transport>>> {
            Box::pin($factory(ctx))
        }

        async fn $factory(ctx: &TransportBuildCtx) -> Result<Arc<dyn Transport>> {
            info!(transport = $log_name, $log_msg);
            // Credential parsing (incl. env fallback) is adapter-owned.
            let t = <$adapter>::from_credentials(&ctx.im_credentials)?;
            Ok(Arc::new(t) as Arc<dyn Transport>)
        }
    };
}

adapter_entry!(
    telegram_entry,
    telegram_factory,
    TelegramTransport,
    "telegram",
    "building Telegram Bot API transport"
);
adapter_entry!(
    wecom_entry,
    wecom_factory,
    WecomTransport,
    "wecom",
    "building WeCom Bot WebSocket transport"
);
adapter_entry!(
    feishu_entry,
    feishu_factory,
    FeishuTransport,
    "feishu",
    "building Feishu WebSocket transport"
);
adapter_entry!(
    discord_entry,
    discord_factory,
    DiscordTransport,
    "discord",
    "building Discord Gateway WebSocket transport"
);

fn ilink_entry(ctx: &TransportBuildCtx) -> BoxFuture<'_, Result<Arc<dyn Transport>>> {
    Box::pin(ilink_factory(ctx))
}

fn null_entry(ctx: &TransportBuildCtx) -> BoxFuture<'_, Result<Arc<dyn Transport>>> {
    Box::pin(null_factory(ctx))
}

fn mcp_ilink_entry(ctx: &TransportBuildCtx) -> BoxFuture<'_, Result<Arc<dyn Transport>>> {
    Box::pin(async move {
        // MCP subprocess path: the manager hands over a pre-resolved hub URL +
        // token (no interactive registration). Same env-key contract as the
        // other MCP kinds: IM_AGENTPROC_MCP_ILINK_HUB_URL / _ILINK_TOKEN.
        let hub_url = ctx
            .im_credentials
            .get("hub_url")
            .context("IM_AGENTPROC_MCP_ILINK_HUB_URL is required")?;
        let token = ctx
            .im_credentials
            .get("token")
            .context("IM_AGENTPROC_MCP_ILINK_TOKEN is required")?;
        Ok(Arc::new(
            IlinkTransport::new(hub_url.clone(), token.clone()).context("build iLink transport")?,
        ) as Arc<dyn Transport>)
    })
}

async fn null_factory(ctx: &TransportBuildCtx) -> Result<Arc<dyn Transport>> {
    Ok(Arc::new(NullTransport::new(ctx.kind.clone())))
}

/// Built-in iLink adapter: `via: hub` resolves a virtual token through the Hub,
/// `via: direct` connects to the real iLink upstream (see `resolve_direct_base_url`).
async fn ilink_factory(ctx: &TransportBuildCtx) -> Result<Arc<dyn Transport>> {
    let t: Arc<dyn Transport> = match ctx.via {
        Via::Hub => {
            let (hub_url, token) = resolve_hub_connection(
                &ctx.hub_url,
                ctx.explicit_token.as_deref(),
                ctx.cred_file.as_deref(),
                ctx.force_pair,
                ctx.register_name.as_deref(),
                ctx.force_register,
                ctx.config_path.as_deref(),
                ctx.description.as_deref(),
            )
            .await?;
            info!(%hub_url, via = "hub", "using Hub base URL for downstream");
            Arc::new(IlinkTransport::new(hub_url, token).context("build iLink transport")?)
        }
        Via::Direct => {
            // YAML `base_url:` overrides the CLI/env URL for this profile, so a
            // bridge manager can mix hub and direct profiles against different
            // upstreams. Without `base_url:`, require a non-default `--hub-url` /
            // `WEIXIN_BASE_URL` — refusing the localhost Hub default avoids
            // silently pointing a direct bridge at a Hub (review M2).
            let base = resolve_direct_base_url(ctx.direct_base_url.as_deref(), &ctx.hub_url)?;
            let (base, token) = resolve_direct_connection(
                &base,
                ctx.explicit_token.as_deref(),
                ctx.cred_file.as_deref(),
                ctx.force_pair,
                ctx.force_register,
                ctx.config_path.as_deref(),
                ctx.interactive,
            )
            .await?;
            info!(base = %base, via = "direct", "connecting directly to iLink upstream");
            // direct mode cannot resume CLI sessions across messages (the real
            // upstream does not echo the HubExt session_id the Hub persists).
            info!(
                "via: direct 不支持跨消息 CLI 会话续接（真实上游不回显 session_id）；每条消息起新 CLI 会话。"
            );
            Arc::new(IlinkTransport::new(base, token).context("build iLink transport (direct)")?)
        }
    };
    Ok(t)
}

/// Resolve the iLink upstream base URL for a `via: direct` profile (review M2).
///
/// - A YAML `base_url:` always wins (lets a manager mix hub/direct profiles
///   against different upstreams).
/// - Otherwise the CLI/env `--hub-url` / `WEIXIN_BASE_URL` is used — but the
///   localhost Hub default is rejected, so a direct bridge never silently
///   targets a Hub/localhost and fires `get_bot_qrcode` at the wrong server.
pub fn resolve_direct_base_url(direct_base_url: Option<&str>, cli_hub_url: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(kind: &str, hub_url: &str) -> TransportBuildCtx {
        TransportBuildCtx {
            kind: kind.to_string(),
            via: Via::Hub,
            hub_url: hub_url.to_string(),
            direct_base_url: None,
            im_credentials: HashMap::new(),
            explicit_token: None,
            cred_file: None,
            force_pair: false,
            force_register: false,
            register_name: None,
            config_path: None,
            description: None,
            interactive: false,
            allow_null_placeholder: false,
        }
    }

    #[tokio::test]
    async fn unknown_kind_bails_without_allow_placeholder() {
        let reg = TransportRegistry::with_builtins();
        let err = match reg.build(&ctx("nocloud", DEFAULT_HUB_URL)).await {
            Ok(_) => panic!("expected bail for unknown kind"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nocloud") && msg.contains("--allow-null-transport"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn unknown_kind_with_allow_placeholder_loads_null_transport() {
        let reg = TransportRegistry::with_builtins();
        let mut c = ctx("nocloud", DEFAULT_HUB_URL);
        c.allow_null_placeholder = true;
        let t = reg.build(&c).await.unwrap();
        assert!(!t.capabilities().media_upload);
    }

    #[tokio::test]
    async fn null_kind_is_a_registered_factory() {
        let reg = TransportRegistry::with_builtins();
        let t = reg.build(&ctx("null", DEFAULT_HUB_URL)).await.unwrap();
        assert!(!t.capabilities().media_upload);
    }

    #[tokio::test]
    async fn builtin_im_kinds_are_registered_and_parse_credentials() {
        let reg = TransportRegistry::with_builtins();
        for kind in ["ilink", "telegram", "wecom", "feishu", "discord", "null"] {
            assert!(reg.get(kind).is_some(), "{kind} not registered");
        }
        // Credential parsing is adapter-owned: telegram without token/env fails.
        let err = match reg.build(&ctx("telegram", DEFAULT_HUB_URL)).await {
            Ok(_) => panic!("expected credential bail"),
            Err(e) => e,
        };
        let msg = format!("{err:#}").to_lowercase();
        assert!(msg.contains("token") && msg.contains("telegram"), "{msg}");
    }

    #[test]
    fn custom_factory_registration_is_picked_up() {
        let mut reg = TransportRegistry::with_builtins();
        reg.register("echo", |ctx| {
            Box::pin(async move {
                Ok(Arc::new(NullTransport::new(ctx.kind.clone())) as Arc<dyn Transport>)
            })
        });
        assert!(reg.get("echo").is_some());
        assert!(reg.kinds().contains(&"echo"));
    }

    #[test]
    fn direct_base_url_guards_match_m2_contract() {
        let base =
            resolve_direct_base_url(Some("https://ilinkai.weixin.qq.com"), DEFAULT_HUB_URL).unwrap();
        assert_eq!(base, "https://ilinkai.weixin.qq.com");
        let err = resolve_direct_base_url(Some("   "), "https://up.example.com").unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
        let err = resolve_direct_base_url(None, DEFAULT_HUB_URL).unwrap_err();
        assert!(format!("{err:#}").contains("via: direct"));
    }
}
