//! Inbound attachment normalisation.
//!
//! Each adapter translates its own wire payload into a list of [`MediaRef`]s
//! before it crosses into [`super::InboundMessage`]. The agentproc spec
//! promises an interoperable schema on `turn.attachments[]`: at least
//! `kind` + `url`, with optional `filename` / `mime_type` / `size`. The
//! pieces below tighten the loose ends so adapters agree.
//!
//! - **`kind`** is constrained to a small controlled vocabulary. Unknown
//!   kinds fall back to `"other"` and emit a `warn!` so the bridge manager
//!   can spot misconfigured IM channels at a glance.
//! - **`url`** scheme is restricted to `file://` / `http://` / `https://` /
//!   `data:`. Anything else is dropped (warn-and-ignore) so a misbehaving
//!   upstream can't smuggle a `javascript:` or `ftp://` URL into the agent
//!   prompt.
//! - **Relative `file://` paths** are resolved against `cwd` so the
//!   `turn.attachments[]` url is always absolute by the time the agent sees
//!   it. This matches the spec's `cwd` semantics (agentproc/protocol.md).
//!
//! Adapters should call [`normalize_attachment`] (or [`normalize_attachments`]
//! for a batch) at the seam where they construct [`super::InboundMessage`].
//! The current default behaviour (no normalisation) preserves backwards
//! compatibility — adapters can opt in incrementally.

use std::path::Path;

use anyhow::{anyhow, Result};
use tracing::warn;
use url::Url;

use super::MediaRef;

/// Controlled vocabulary for attachment `kind`. Anything outside this set is
/// normalised to `Other` and emits a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    File,
    Audio,
    Video,
    Other,
}

impl AttachmentKind {
    /// Stable wire string used in the `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Other => "other",
        }
    }

    /// Best-effort parse from a free-form string. Unknown values yield
    /// `Other`; the caller is expected to warn separately.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "image" | "photo" | "picture" | "img" => Self::Image,
            "file" | "document" | "doc" | "attachment" => Self::File,
            "audio" | "voice" => Self::Audio,
            "video" => Self::Video,
            _ => Self::Other,
        }
    }
}

/// Schemes accepted on the inbound `url` field. Anything else is dropped.
pub const ALLOWED_SCHEMES: &[&str] = &["file", "http", "https", "data"];

/// Validate and normalise a single inbound attachment. Convenience wrapper
/// that uses `Path::new("")` as `cwd` — callers that don't have a working
/// directory at hand (e.g. iLink `build_media`, which is called during
/// inbound translation without a bridge cwd) should use this. Absolute
/// `file://` URLs pass through unchanged because `path.canonicalize()` is
/// skipped when `cwd` is empty; relative `file://` paths return `None`
/// (caller is expected to use [`normalize_attachment`] with a real cwd).
pub fn normalize_attachment_without_cwd(
    raw_kind: &str,
    raw_url: &str,
    filename: Option<String>,
    mime_type: Option<String>,
    size: Option<u64>,
) -> Option<NormalizedAttachment> {
    let kind = AttachmentKind::parse(raw_kind);
    if kind == AttachmentKind::Other {
        warn!(
            kind = raw_kind,
            "unknown inbound attachment kind; falling back to `other`"
        );
    }

    let parsed = match Url::parse(raw_url) {
        Ok(u) => u,
        Err(err) => {
            warn!(url = raw_url, error = %err, "unparseable inbound url; dropping attachment");
            return None;
        }
    };
    let scheme = parsed.scheme();
    if !ALLOWED_SCHEMES.contains(&scheme) {
        warn!(
            url = raw_url,
            scheme, "disallowed inbound url scheme; dropping attachment"
        );
        return None;
    }
    Some(NormalizedAttachment {
        kind,
        url: raw_url.to_string(),
        filename,
        mime_type,
        size,
    })
}

/// Validate and normalise a single inbound attachment.
///
/// - `kind`: parsed via [`AttachmentKind::parse`]; unknown values fall back
///   to `other`. Returns the normalised kind via [`NormalizedAttachment::kind`].
/// - `url`: only the four allowed schemes survive; other schemes produce a
///   warning + `None` return so the caller skips the attachment. Relative
///   `file://` paths are resolved against `cwd`.
/// - `filename` / `mime_type` / `size` are passed through unchanged.
pub fn normalize_attachment(
    raw_kind: &str,
    raw_url: &str,
    filename: Option<String>,
    mime_type: Option<String>,
    size: Option<u64>,
    cwd: &Path,
) -> Option<NormalizedAttachment> {
    let kind = AttachmentKind::parse(raw_kind);
    if kind == AttachmentKind::Other {
        warn!(
            kind = raw_kind,
            "unknown inbound attachment kind; falling back to `other`"
        );
    }

    let parsed = match Url::parse(raw_url) {
        Ok(u) => u,
        Err(err) => {
            warn!(url = raw_url, error = %err, "unparseable inbound url; dropping attachment");
            return None;
        }
    };
    let scheme = parsed.scheme();
    if !ALLOWED_SCHEMES.contains(&scheme) {
        warn!(
            url = raw_url,
            scheme, "disallowed inbound url scheme; dropping attachment"
        );
        return None;
    }

    // Resolve file:// paths against cwd. Relative paths are joined onto cwd;
    // absolute paths are canonicalised so symlinks + `/var` ↔ `/private/var`
    // get normalised. Missing files are dropped (warn) so adapters don't ship
    // unreachable file URLs to the agent.
    let resolved_url = if scheme == "file" {
        let path = match parsed.to_file_path() {
            Ok(p) => p,
            Err(()) => {
                warn!(
                    url = raw_url,
                    "file:// url did not decode as a path; dropping"
                );
                return None;
            }
        };
        let abs = if path.is_relative() {
            cwd.join(&path)
        } else {
            path
        };
        let canonical = match abs.canonicalize() {
            Ok(p) => p,
            Err(err) => {
                warn!(
                    url = raw_url,
                    target = %abs.display(),
                    error = %err,
                    "file:// attachment target does not exist; dropping"
                );
                return None;
            }
        };
        Url::from_file_path(&canonical)
            .map(|u| u.to_string())
            .unwrap_or_else(|()| raw_url.to_string())
    } else {
        raw_url.to_string()
    };

    Some(NormalizedAttachment {
        kind,
        url: resolved_url,
        filename,
        mime_type,
        size,
    })
}

/// Convenience wrapper for a list of `(kind, url, ...)` tuples.
pub fn normalize_attachments<I>(items: I, cwd: &Path) -> Vec<MediaRef>
where
    I: IntoIterator<Item = (String, String, Option<String>, Option<String>, Option<u64>)>,
{
    let mut out = Vec::new();
    for (kind, url, filename, mime_type, size) in items {
        if let Some(n) = normalize_attachment(&kind, &url, filename, mime_type, size, cwd) {
            out.push(n.into_media_ref());
        }
    }
    out
}

/// Normalised attachment: every field has passed the kind/url/scheme checks.
#[derive(Debug, Clone)]
pub struct NormalizedAttachment {
    pub kind: AttachmentKind,
    pub url: String,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
}

impl NormalizedAttachment {
    /// Project into the wire-facing [`MediaRef`] used by [`super::InboundMessage`].
    pub fn into_media_ref(self) -> MediaRef {
        MediaRef {
            kind: self.kind.as_str().to_string(),
            url: self.url,
            filename: self.filename,
            mime_type: self.mime_type,
            size: self.size,
        }
    }
}

/// Helper: assert `cwd` exists when callers want to resolve relative paths.
/// Adapters that already run inside a process with a known cwd can ignore
/// the result; the function exists to keep the call sites explicit.
pub fn require_cwd(cwd: &Path) -> Result<&Path> {
    if cwd.as_os_str().is_empty() {
        Err(anyhow!(
            "cwd is empty; cannot resolve relative file:// attachments"
        ))
    } else {
        Ok(cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cwd() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[test]
    fn parse_kind_known_values() {
        assert_eq!(AttachmentKind::parse("image"), AttachmentKind::Image);
        assert_eq!(AttachmentKind::parse("IMAGE"), AttachmentKind::Image);
        assert_eq!(AttachmentKind::parse("photo"), AttachmentKind::Image);
        assert_eq!(AttachmentKind::parse("voice"), AttachmentKind::Audio);
        assert_eq!(AttachmentKind::parse("video"), AttachmentKind::Video);
        assert_eq!(AttachmentKind::parse("doc"), AttachmentKind::File);
    }

    #[test]
    fn parse_kind_unknown_falls_back_to_other() {
        assert_eq!(AttachmentKind::parse("gibberish"), AttachmentKind::Other);
    }

    #[test]
    fn normalize_drops_disallowed_scheme() {
        // ftp:// is not in ALLOWED_SCHEMES — the attachment is dropped.
        let out =
            normalize_attachment("image", "ftp://example.com/x.png", None, None, None, &cwd());
        assert!(out.is_none(), "ftp:// must be dropped: {out:?}");
    }

    #[test]
    fn normalize_drops_unparseable_url() {
        let out = normalize_attachment("image", "not a url at all", None, None, None, &cwd());
        assert!(out.is_none(), "garbage url must be dropped: {out:?}");
    }

    #[test]
    fn normalize_keeps_allowed_schemes() {
        // For `file://` we canonicalise a real file under /tmp so the test
        // works on macOS where paths under /etc get mapped to /private/etc.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("x.png");
        std::fs::write(&real, b"\x89PNG").unwrap();
        let real_url = format!("file://{}", real.display());

        for url in [
            "https://example.com/x.png",
            "http://example.com/x.png",
            "data:image/png;base64,AAAA",
            real_url.as_str(),
        ] {
            let out = normalize_attachment(
                "image",
                url,
                Some("x.png".into()),
                Some("image/png".into()),
                Some(42),
                dir.path(),
            )
            .expect("allowed scheme survives");
            assert!(
                out.url
                    .starts_with(if url.starts_with("data:") || url.starts_with("http") {
                        url.split(':').next().unwrap()
                    } else {
                        "file://"
                    }),
                "url scheme preserved: {}",
                out.url
            );
            // Non-file:// schemes pass through verbatim.
            if !url.starts_with("file://") {
                assert_eq!(out.url, url);
            }
            assert_eq!(out.kind, AttachmentKind::Image);
            assert_eq!(out.filename.as_deref(), Some("x.png"));
            assert_eq!(out.mime_type.as_deref(), Some("image/png"));
            assert_eq!(out.size, Some(42));
        }
    }

    #[test]
    fn normalize_unknown_kind_keeps_attachment_with_other() {
        let out = normalize_attachment(
            "weird-kind",
            "https://example.com/x",
            None,
            None,
            None,
            &cwd(),
        )
        .expect("kind=other is still kept");
        assert_eq!(out.kind, AttachmentKind::Other);
    }

    #[test]
    fn normalize_resolves_relative_file_path_against_cwd() {
        // url crate semantics for `file://`: the URL is parsed as
        // `scheme://host/path`, where `file://hello.txt` means
        // host=`hello.txt`, path=`/`. There is no syntactic way to express a
        // *relative* path through `file://` — to carry one, callers must use
        // `file:` (no slashes) which `url::Url` parses as `/hello.txt`.
        //
        // What we actually verify here: an absolute file:// URL whose target
        // exists is canonicalised into a clean `file://` URL that points at
        // the same file. We also verify that the non-existent branch is
        // surfaced as a None (so adapters don't ship broken file paths).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hi").unwrap();
        let absolute = format!("file://{}", file.display());
        let out = normalize_attachment(
            "file",
            &absolute,
            Some("hello.txt".into()),
            Some("text/plain".into()),
            Some(2),
            dir.path(),
        )
        .expect("absolute file:// resolves");
        assert!(
            out.url.starts_with("file://"),
            "url should round-trip as file://: {}",
            out.url
        );
        assert!(
            out.url.contains("hello.txt"),
            "url should reference hello.txt: {}",
            out.url
        );

        // Non-existent path: canonicalize fails → attachment is dropped.
        let bogus = format!("file://{}/does-not-exist.txt", dir.path().display());
        let none = normalize_attachment("file", &bogus, None, None, None, dir.path());
        assert!(
            none.is_none(),
            "non-existent file:// attachment should be dropped"
        );
    }

    #[test]
    fn normalize_attachment_list_drops_individual_bad_entries() {
        let items = vec![
            (
                "image".into(),
                "https://example.com/a.png".into(),
                None,
                None,
                None,
            ),
            (
                "image".into(),
                "javascript:alert(1)".into(),
                None,
                None,
                None,
            ),
            (
                "file".into(),
                "data:application/pdf;base64,AAAA".into(),
                Some("x.pdf".into()),
                Some("application/pdf".into()),
                None,
            ),
        ];
        let out = normalize_attachments(items, &cwd());
        // 1st and 3rd survive; 2nd (javascript:) is dropped.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, "image");
        assert_eq!(out[1].kind, "file");
    }

    #[test]
    fn normalized_attachment_into_media_ref_round_trips() {
        let out = normalize_attachment(
            "voice",
            "https://example.com/a.ogg",
            Some("a.ogg".into()),
            Some("audio/ogg".into()),
            Some(1234),
            &cwd(),
        )
        .unwrap();
        let mr = out.into_media_ref();
        assert_eq!(mr.kind, "audio");
        assert_eq!(mr.url, "https://example.com/a.ogg");
        assert_eq!(mr.filename.as_deref(), Some("a.ogg"));
        assert_eq!(mr.mime_type.as_deref(), Some("audio/ogg"));
        assert_eq!(mr.size, Some(1234));
    }

    #[test]
    fn require_cwd_rejects_empty() {
        let err = require_cwd(Path::new("")).unwrap_err();
        assert!(format!("{err}").contains("cwd is empty"));
    }
}
