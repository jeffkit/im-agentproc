use thiserror::Error;

/// Error type carried across the bridge's iLink transport boundary.
///
/// Trimmed from `ilink-hub::error::HubError` per the split (proposal
/// Appendix A): only the upstream/IO variants the bridge actually uses are
/// retained. Hub-only variants (`Database`, `ClientNotFound`, `SessionNotFound`,
/// `Config`, `Timeout`, `QueueBackend`, `InvalidToken`) are NOT carried over.
#[derive(Debug, Error)]
pub enum HubError {
    /// HTTP-level failure communicating with the iLink upstream.
    #[error("iLink upstream HTTP error {status}: {msg}")]
    UpstreamHttp { status: u16, msg: String },

    /// Response from the iLink upstream could not be parsed.
    #[error("iLink upstream parse error: {0}")]
    UpstreamParse(String),

    /// Generic upstream error — kept for callers that may not have a more
    /// specific variant available.
    #[error("iLink upstream error: {0}")]
    Upstream(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<anyhow::Error> for HubError {
    fn from(e: anyhow::Error) -> Self {
        // Recover a `HubError` that was wrapped at an upstream call site via
        // `anyhow::Error::new(HubError::UpstreamHttp { ... })` or
        // `HubError::UpstreamParse(...)`. This lets N-06 specific variants
        // survive a round-trip through `anyhow::Result` and still be
        // pattern-matched by downstream consumers (e.g. to distinguish a
        // transient HTTP 503 from a malformed JSON body).
        match e.downcast::<HubError>() {
            Ok(hub_err) => hub_err,
            Err(e) => HubError::Upstream(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N-06: UpstreamHttp Display includes both the status code and the message.
    #[test]
    fn upstream_http_display_includes_status_and_msg() {
        let err = HubError::UpstreamHttp {
            status: 503,
            msg: "service unavailable".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("503"), "status missing from Display: {s}");
        assert!(
            s.contains("service unavailable"),
            "msg missing from Display: {s}"
        );
    }

    #[test]
    fn upstream_parse_display_includes_message() {
        let err = HubError::UpstreamParse("unexpected token at line 3".to_string());
        let s = err.to_string();
        assert!(
            s.contains("unexpected token at line 3"),
            "msg missing from Display: {s}"
        );
    }

    #[test]
    fn from_anyhow_preserves_upstream_http_via_downcast() {
        let original = HubError::UpstreamHttp {
            status: 429,
            msg: "rate limited".to_string(),
        };
        let wrapped: anyhow::Error = anyhow::Error::new(original);
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::UpstreamHttp { status, msg } => {
                assert_eq!(status, 429);
                assert_eq!(msg, "rate limited");
            }
            other => panic!("expected UpstreamHttp, got {other:?}"),
        }
    }

    #[test]
    fn from_anyhow_preserves_upstream_parse_via_downcast() {
        let original = HubError::UpstreamParse("bad json".to_string());
        let wrapped: anyhow::Error = anyhow::Error::new(original);
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::UpstreamParse(msg) => assert_eq!(msg, "bad json"),
            other => panic!("expected UpstreamParse, got {other:?}"),
        }
    }

    #[test]
    fn from_anyhow_collapses_other_errors_to_upstream_string() {
        let wrapped: anyhow::Error = anyhow::anyhow!("raw anyhow message");
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::Upstream(s) => assert_eq!(s, "raw anyhow message"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn upstream_http_status_zero_is_legal_for_pre_send_failures() {
        let err = HubError::UpstreamHttp {
            status: 0,
            msg: "connection refused".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("connection refused"), "msg missing: {s}");
    }
}
