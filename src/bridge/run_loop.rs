//! Library entry for the default bridge run: transport building + reconnect
//! loop + signal handling.
//!
//! The `im-agentproc` binary delegates here with the built-in registry, so a
//! downstream crate can ship a thin `main` that registers its **own** transport
//! factories into a [`TransportRegistry`] and delegates to
//! [`run_bridge_reconnecting`] — adding a `transport:` kind with zero edits to
//! this repository (see `docs/transport.md`).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::transport::registry::{TransportBuildCtx, TransportRegistry};
use super::transport::Transport;
use super::{run_bridge_with_shutdown, BridgeApp, BridgeStop, Via};
use super::transport::connection::{default_direct_credential_path, default_local_credential_path};

/// Everything the default bridge run needs. Plain values — no CLI types — so
/// downstream crates can construct it without depending on our binary.
pub struct BridgeRunOptions {
    /// Loaded bridge profile (one YAML == one agentproc profile).
    pub app: BridgeApp,
    /// Profile YAML path (stored alongside resolved credentials).
    pub config_path: PathBuf,
    /// Transport factories to resolve `transport:` against. Use
    /// [`TransportRegistry::with_builtins()`] and/or `register` your own.
    pub registry: TransportRegistry,
    /// Hub base URL (`--hub-url` / `WEIXIN_BASE_URL`).
    pub hub_url: String,
    /// Explicit `--token` / `WEIXIN_TOKEN`, trimmed and empty-filtered.
    pub explicit_token: Option<String>,
    /// `--cred-file` override for the saved credential JSON.
    pub cred_file: Option<String>,
    /// `--pair`: force a fresh QR pairing even if credentials exist.
    pub force_pair: bool,
    /// `--force-register`: discard unusable saved credentials and re-register.
    pub force_register: bool,
    /// `--register-name`: client name for automatic Hub registration.
    pub register_name: Option<String>,
    /// Accept a `NullTransport` placeholder for unregistered kinds
    /// (`--allow-null-transport`).
    pub allow_null_transport: bool,
    /// Disable interactive flows (QR login) even on a TTY.
    pub no_interactive: bool,
}

impl BridgeRunOptions {
    fn explicit_token(&self) -> Option<&str> {
        self.explicit_token.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// Build the configured transport for the current run.
    ///
    /// Transport selection goes through the caller-provided registry: each
    /// `transport:` kind resolves to a registered factory (built-ins: ilink
    /// hub/direct, telegram, wecom, feishu, discord; each adapter owns its
    /// credential parsing), and an unknown kind fails fast unless
    /// `allow_null_transport` opts into the `NullTransport` placeholder (L4).
    async fn build_transport(
        &self,
        description: Option<&str>,
        interactive: bool,
    ) -> Result<Arc<dyn Transport>> {
        let app = &self.app;
        let ctx = TransportBuildCtx {
            kind: app.transport().as_str().to_string(),
            via: app.via(),
            hub_url: self.hub_url.clone(),
            direct_base_url: app.direct_base_url().map(str::to_string),
            im_credentials: app.im_credentials().clone(),
            explicit_token: self.explicit_token().map(str::to_string),
            cred_file: self.cred_file.clone(),
            force_pair: self.force_pair,
            force_register: self.force_register,
            register_name: self.register_name.clone(),
            config_path: Some(self.config_path.clone()),
            description: description.map(str::to_string),
            interactive,
            allow_null_placeholder: self.allow_null_transport,
        };
        let t = self.registry.build(&ctx).await?;
        let caps = t.capabilities();
        info!(
            transport = ctx.kind.as_str(),
            media_upload = caps.media_upload,
            "transport built"
        );
        Ok(t)
    }
}

/// Run the default bridge mode: probe profiles, build the transport, long-poll
/// for messages, and reconnect on runtime token revocation until Ctrl-C /
/// SIGTERM. Returns when the process should exit.
pub async fn run_bridge_reconnecting(opts: BridgeRunOptions) -> Result<()> {
    let app = opts.app.clone();

    // Startup probe to verify that the CLI command(s) exist and are usable.
    for name in app.profile_names() {
        if let Some(profile) = app.profile(name) {
            if let Err(e) = crate::bridge::probe_profile_light(profile) {
                anyhow::bail!("Startup probe failed for profile `{name}`: {e}");
            }
        }
    }

    let cred_path = opts
        .cred_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| match app.via() {
            Via::Direct => default_direct_credential_path(),
            Via::Hub => default_local_credential_path(),
        });
    let using_explicit_token = opts.explicit_token().is_some();

    // Interactive flows (QR login) require a TTY for stdout and must not
    // be disabled via --no-interactive / ILINKHUB_BRIDGE_NON_INTERACTIVE
    // (the manager injects the latter so its children fail fast instead
    // of QR-blocking headless — review N1).
    let interactive = !opts.no_interactive && std::io::stdout().is_terminal();

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

        let transport = opts.build_transport(description, interactive).await?;

        let mut handle =
            tokio::spawn(run_bridge_with_shutdown(transport, app.clone(), shutdown.clone()));

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    fn write_yaml(dir: &Path, content: &str) -> PathBuf {
        let p = dir.join("profile.yaml");
        std::fs::write(&p, content).unwrap();
        p
    }

    fn opts_for(cfg: &Path, hub_url: &str, allow_null: bool) -> BridgeRunOptions {
        BridgeRunOptions {
            app: BridgeApp::load(cfg).unwrap(),
            config_path: cfg.to_path_buf(),
            registry: TransportRegistry::with_builtins(),
            hub_url: hub_url.to_string(),
            explicit_token: None,
            cred_file: None,
            force_pair: false,
            force_register: false,
            register_name: None,
            allow_null_transport: allow_null,
            no_interactive: true,
        }
    }

    #[tokio::test]
    async fn build_transport_direct_bails_without_base_url() {
        // M2: build_transport on a via: direct profile with no base_url and the
        // default hub-url bails at the M2 gate before any network call.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\nvia: direct\n",
        );
        let opts = opts_for(&cfg, "http://127.0.0.1:8765", false);
        let err = match opts.build_transport(None, true).await {
            Ok(_) => panic!("expected M2 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("via: direct") && msg.contains("base_url"),
            "expected M2 bail: {msg}"
        );
    }

    #[tokio::test]
    async fn build_transport_non_ilink_transport_bails_without_allow_flag() {
        // L4: a transport with no registered factory fails fast unless the
        // placeholder is explicitly allowed.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: foobar-unknown\n",
        );
        let opts = opts_for(&cfg, "http://127.0.0.1:8765", false);
        let err = match opts.build_transport(None, true).await {
            Ok(_) => panic!("expected L4 bail, got transport"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foobar-unknown") && msg.contains("--allow-null-transport"),
            "expected L4 bail mentioning transport + flag: {msg}"
        );
    }

    #[tokio::test]
    async fn build_transport_unknown_transport_succeeds_with_allow_flag() {
        // With the placeholder allowed, an unknown transport loads a
        // NullTransport instead of failing — the escape hatch works.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: foobar-unknown\n",
        );
        let opts = opts_for(&cfg, "http://127.0.0.1:8765", true);
        let t = opts.build_transport(None, true).await.unwrap();
        // NullTransport placeholder: constructs and reports no media upload.
        assert!(!t.capabilities().media_upload);
    }

    #[tokio::test]
    async fn downstream_registry_injection_is_honoured() {
        // The whole point of BridgeRunOptions.registry: a downstream crate
        // registers a custom factory and build_transport picks it up without
        // any edits to the shipped binary.
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_yaml(
            dir.path(),
            "agentproc:\n  command: echo\n  args: [\"ok\"]\ntransport: rocketchat\n",
        );
        let mut opts = opts_for(&cfg, "http://127.0.0.1:8765", false);
        opts.registry.register("rocketchat", |ctx| {
            Box::pin(async move {
                use crate::bridge::transport::NullTransport;
                Ok(Arc::new(NullTransport::new(ctx.kind.clone())) as Arc<dyn Transport>)
            })
        });
        let t = opts.build_transport(None, true).await.unwrap();
        // The custom factory (not the fail-fast path) produced the transport.
        assert!(!t.capabilities().media_upload);
    }
}
