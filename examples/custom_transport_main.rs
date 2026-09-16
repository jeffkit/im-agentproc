//! Downstream "thin main" example: register a custom transport factory and
//! delegate the whole bridge run (probe → transport → long-poll → reconnect →
//! signals) to the library.
//!
//! A downstream crate depends on `im-agentproc` and ships its own binary with
//! roughly this content — zero edits to this repository. Build/run:
//!
//! ```text
//! cargo run --example custom_transport_main
//! ```
//!
//! (This in-repo example fakes the adapter with a `NullTransport`; a real
//! downstream crate would implement `Transport` for its IM.)

use std::sync::Arc;

use anyhow::Result;
use im_agentproc::bridge::run_loop::{run_bridge_reconnecting, BridgeRunOptions};
use im_agentproc::bridge::transport::registry::TransportRegistry;
use im_agentproc::bridge::transport::{NullTransport, Transport};
use im_agentproc::paths::default_bridge_config_path;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("im_agentproc=info")),
        )
        .init();

    // 1. Start from the built-in adapters (ilink/telegram/wecom/feishu/discord/null)…
    let mut registry = TransportRegistry::with_builtins();

    // 2. …and register your own channel. Credential/env parsing stays in the
    //    adapter; the factory receives a TransportBuildCtx with the profile
    //    (`im_credentials` already ${VAR}-expanded) and CLI-derived options.
    registry.register("rocketchat", |ctx| {
        Box::pin(async move {
            let token = ctx
                .im_credentials
                .get("token")
                .cloned()
                .or_else(|| std::env::var("ROCKETCHAT_TOKEN").ok())
                .expect("rocketchat needs im_credentials.token");
            Ok(Arc::new(NullTransport::new(token)) as Arc<dyn Transport>)
        })
    });

    // 3. Delegate to the shared run loop.
    let config_path = default_bridge_config_path();
    let app = im_agentproc::bridge::BridgeApp::load(&config_path)?;
    run_bridge_reconnecting(BridgeRunOptions {
        app,
        config_path,
        registry,
        hub_url: std::env::var("WEIXIN_BASE_URL").unwrap_or_default(),
        explicit_token: std::env::var("WEIXIN_TOKEN").ok(),
        cred_file: None,
        force_pair: false,
        force_register: false,
        register_name: None,
        allow_null_transport: false,
        no_interactive: false,
    })
    .await
}
