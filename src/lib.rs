//! im-agentproc — IM-side runtime for the agentproc ecosystem.
//!
//! Connects an IM transport (iLink/WeChat today; more IMs via the `Transport`
//! trait later) to agentproc profiles: one inbound IM text message → one
//! agentproc profile run.
//!
//! Extracted from `ilink-hub`'s `src/bridge/` subtree; see
//! `docs/proposals/bridge-as-multi-im-runtime.md` (Appendix A) for the split
//! rationale and the remaining in-crate decoupling work.

pub mod bridge;
pub mod client;
pub mod error;
pub mod ilink;
pub mod mcp;
pub mod paths;

pub use error::HubError;
