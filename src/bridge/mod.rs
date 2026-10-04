//! CLI bridge: connect to iLink Hub as a virtual-token backend and run one
//! agentproc profile (one YAML file == one profile) per text message.
//!
//! Used by the `im-agentproc` binary; see `docs/bridge/index.md`.

pub mod builtin;
pub mod config;
pub(crate) mod dispatcher;
mod executor;
pub mod manager;
mod paths;
mod probe;
pub mod protocol;
pub mod run_loop;
pub mod transport;
pub mod vtoken_env;

pub use config::{BridgeApp, BridgeProfile, BridgeProfileFile, TransportKind, Via};
pub use dispatcher::{run_bridge, run_bridge_with_shutdown, BridgeStop};
pub use executor::MAX_CLI_CAPTURE_BYTES;
pub use paths::resolve_bridge_executable;
pub use probe::{
    check_command_exists, dry_run_profile, find_in_path_robust, probe_profile_light, ProbeError,
};
pub use protocol::PROTOCOL_VERSION;
pub use transport::connection::{
    default_auto_client_name, default_direct_credential_path, default_local_credential_path,
    hub_response_token_rejected, resolve_direct_connection, resolve_hub_connection,
    validate_hub_token,
};

/// Full phrases that identify a credential failure on their own (no bare words:
/// `token` also matches `max_tokens`, `auth` also matches `author`). Everything
/// matched here stops the whole bridge process, so only unambiguous wording counts.
pub const AUTH_ERROR_KEYWORDS: &[&str] = &[
    "not logged in",
    "invalid api key",
    "incorrect api key",
    "missing api key",
    "unauthorized",
    "unauthenticated",
    "authentication failed",
    "authentication required",
    "authentication error",
    "invalid credentials",
    "credentials expired",
    "sign in",
    "keychain",
];

/// True when the CLI's error text names a *credential* failure. The text is
/// normally produced by the model or an upstream service, so only full phrases count.
pub fn is_fatal_auth_error(cli_error_text: &str) -> bool {
    let text = cli_error_text.to_lowercase();
    AUTH_ERROR_KEYWORDS.iter().any(|k| text.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_side_errors_are_not_auth_failures() {
        for text in [
            "API error: max_tokens(4096) reached",
            "context tokens exceeded (200001 > 200000)",
            "model not found: claude-nonexistent-9",
            "tool not found: mcp__demo__missing",
            "upstream responded 404 not found",
            "no such file or directory",
            "output_tokens limit",
        ] {
            assert!(
                !is_fatal_auth_error(text),
                "{text:?} is not an auth failure"
            );
        }
    }

    #[test]
    fn genuine_credential_failures_are_fatal() {
        for text in [
            "invalid api key provided",
            "not logged in: run `login` first",
            "401 unauthorized",
        ] {
            assert!(is_fatal_auth_error(text), "{text:?} must be fatal");
        }
    }

    #[test]
    fn overbroad_short_keywords_are_gone() {
        assert!(!AUTH_ERROR_KEYWORDS.iter().any(|k| matches!(
            *k,
            "token" | "auth" | "401" | "login" | "logout" | "credential"
        )));
    }
}
