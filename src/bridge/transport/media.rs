//! 媒体文件下载工具。
//!
//! 各 IM Transport 适配器共用：把 IM 平台媒体下载到进程临时目录，
//! 以 `file:///tmp/...` URL 的形式填入 [`MediaRef`]，
//! 让 agentproc 进程通过文件路径直接读取，无需传递 IM 凭据。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::Builder as TempBuilder;
use tracing::{debug, warn};

use super::MediaRef;

/// 把 `url` 的内容下载到进程内临时文件，返回填好字段的 [`MediaRef`]。
///
/// - `auth_header`: 可选的 `Authorization` 头（如 `Bearer {token}`）。
/// - `filename`: 用于推断扩展名和填 `filename` 字段；可为 `None`。
/// - `mime_type`: 填入 `mime_type` 字段；可为 `None`（由 Content-Type 推断）。
/// - `kind`: `"image"` / `"file"` / `"audio"` / `"video"` 等。
pub async fn download_to_temp(
    http: &reqwest::Client,
    url: &str,
    auth_header: Option<&str>,
    kind: &str,
    filename: Option<&str>,
    mime_type: Option<&str>,
) -> Result<MediaRef> {
    debug!(url, kind, "downloading IM media to temp file");

    let mut req = http.get(url).timeout(Duration::from_secs(60));
    if let Some(auth) = auth_header {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("download {url}: HTTP {}", resp.status());
    }

    // Try to detect extension from filename or Content-Type
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());

    let effective_mime = mime_type
        .map(|s| s.to_string())
        .or_else(|| content_type.clone());

    let ext = ext_from_filename_or_mime(filename, effective_mime.as_deref());
    let size: Option<u64> = resp.content_length();

    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;

    let tmp = write_temp_file(&bytes, ext.as_deref(), kind)?;
    let file_url = format!("file://{}", tmp.display());

    Ok(MediaRef {
        kind: kind.to_string(),
        url: file_url,
        filename: filename.map(|s| s.to_string()),
        mime_type: effective_mime,
        size: size.or(Some(bytes.len() as u64)),
    })
}

/// Write raw bytes to a named temp file and leak its path (so the file
/// survives until process exit).
fn write_temp_file(bytes: &[u8], ext: Option<&str>, kind: &str) -> Result<PathBuf> {
    let prefix = format!("im-{kind}-");
    let suffix = ext.map(|e| format!(".{e}")).unwrap_or_default();
    let mut file = TempBuilder::new()
        .prefix(&prefix)
        .suffix(&suffix)
        .tempfile()
        .context("create temp file for IM media")?;

    use std::io::Write as _;
    file.write_all(bytes).context("write IM media temp file")?;

    // `persist` moves the file to a path that won't be auto-deleted when
    // the NamedTempFile is dropped. The file is cleaned up by the OS on
    // process exit (or by the temp-dir cleaner), which is acceptable for
    // short-lived bridge runs.
    let path = file
        .into_temp_path()
        .keep()
        .context("persist IM media temp file")?;
    Ok(path)
}

/// Guess a file extension from the filename or MIME type.
fn ext_from_filename_or_mime(filename: Option<&str>, mime: Option<&str>) -> Option<String> {
    // Try filename first
    if let Some(name) = filename {
        if let Some(ext) = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
        {
            return Some(ext.to_string());
        }
    }
    // Fall back to MIME
    match mime? {
        "image/jpeg" | "image/jpg" => Some("jpg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        "image/heic" | "image/heif" => Some("heic".to_string()),
        "audio/mpeg" | "audio/mp3" => Some("mp3".to_string()),
        "audio/ogg" => Some("ogg".to_string()),
        "audio/wav" | "audio/x-wav" => Some("wav".to_string()),
        "audio/aac" => Some("aac".to_string()),
        "video/mp4" => Some("mp4".to_string()),
        "video/quicktime" => Some("mov".to_string()),
        "application/pdf" => Some("pdf".to_string()),
        "text/plain" => Some("txt".to_string()),
        other => {
            warn!(mime = other, "unknown MIME type, no extension guessed");
            None
        }
    }
}

/// Read media bytes from any URI scheme we accept on outbound delivery
/// (mirror of [`download_to_temp`] but without persisting to disk: the bytes
/// are uploaded immediately to the IM platform).
///
/// - `data:` URIs must be base64-encoded.
/// - `file://` reads the file from disk.
/// - `http(s)://` fetches via the supplied `reqwest::Client`.
/// - Any other scheme returns an error.
pub async fn read_media_bytes(http: &reqwest::Client, media: &MediaRef) -> Result<Vec<u8>> {
    if let Some(rest) = media.url.strip_prefix("data:") {
        let comma = rest
            .find(',')
            .ok_or_else(|| anyhow::anyhow!("data: url missing comma"))?;
        let header = &rest[..comma];
        let payload = &rest[comma + 1..];
        anyhow::ensure!(
            header.contains(";base64"),
            "data: url must be base64-encoded for send_media"
        );
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(payload)
            .context("decode base64 data: url");
    }
    if let Some(path) = media.url.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("read local media file {path}"));
    }
    if media.url.starts_with("http://") || media.url.starts_with("https://") {
        let resp = http
            .get(&media.url)
            .send()
            .await
            .with_context(|| format!("fetch remote media {}", media.url))?;
        if !resp.status().is_success() {
            anyhow::bail!("remote media fetch HTTP {}", resp.status());
        }
        return resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .context("read remote media bytes");
    }
    anyhow::bail!("unsupported media url scheme: {}", media.url)
}

/// Best-effort filename extracted from a URL's path tail when no explicit
/// `MediaRef.filename` was provided. Returns `None` for bare hostnames
/// (where the path is `/` and the tail equals the host) so callers fall
/// back to a generic default like `"attachment"`.
pub fn filename_from_url(url: &str) -> Option<String> {
    let scheme_sep = url.find("://")?;
    let after_scheme = &url[scheme_sep + 3..];
    let slash = after_scheme.find('/')?;
    let (host, path) = (&after_scheme[..slash], &after_scheme[slash..]);
    let path = path.split('?').next().unwrap_or(path);
    let tail = path.rsplit('/').next().unwrap_or("");
    if tail.is_empty() || tail == host {
        return None;
    }
    Some(tail.to_string())
}
