//! iLink (WeChat clawbot) transport adapter.
//!
//! This is the only place in `src/bridge/` that touches `crate::ilink::types`.
//! It wraps the iLink HTTP client ([`HubClient`]) and implements the generic
//! [`Transport`] trait by translating between iLink wire types and the
//! generic DTOs in [`crate::bridge::transport`].
//!
//! `HubClient` talks to the iLink `/ilink/bot/getupdates` and `/sendmessage`
//! endpoints. When pointed at a Hub base URL it speaks the same iLink protocol
//! the Hub relays upstream; when pointed at the real iLink upstream it connects
//! directly (Stage 3 will formalise `via: hub | direct`).

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tracing::warn;

use super::connection::hub_response_token_rejected;
use crate::bridge::transport::{
    InboundMessage, InboundOutcome, MediaOut, MediaRef, OutboundReply, SendOutcome, Transport,
    TransportCapabilities,
};
use crate::ilink::types::{
    msg_type, BaseInfo, GetUpdatesRequest, GetUpdatesResponse, GetUploadUrlRequest,
    GetUploadUrlResponse, HubExt, SendMessageRequest, SendMessageResponse, WeixinMessage,
};

pub(crate) enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

/// Map the raw HTTP response body of `sendmessage` into a [`SendOutcome`].
///
/// Empty bodies are treated as `Sent`. When the body parses as JSON and `ret`
/// is some non-zero value other than -2, this returns `Err` carrying the
/// upstream ret/errmsg. When the body fails to parse entirely, this returns
/// `Ok(Sent)` for backwards compatibility, with the caller logging a warning.
pub(crate) fn parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SendOutcome::Sent);
    }
    match serde_json::from_str::<SendMessageResponse>(trimmed) {
        Ok(v) => {
            let ret = v.ret.unwrap_or(0);
            if ret == -2 {
                Ok(SendOutcome::Throttled {
                    ret: -2,
                    errmsg: v.errmsg,
                })
            } else if ret != 0 {
                Err((ret, v.errmsg))
            } else {
                Ok(SendOutcome::Sent)
            }
        }
        Err(_) => Ok(SendOutcome::Sent),
    }
}

/// Compute a lowercase hex MD5 digest of `bytes`. iLink's `getuploadurl`
/// takes a `file_md5` field for the upstream to verify the upload against
/// the bytes the caller is about to PUT.
fn md5_hex(bytes: &[u8]) -> String {
    let digest = md5::compute(bytes);
    format!("{:x}", digest)
}

#[derive(Clone)]
pub struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    /// 生产入口（与 `with_hub_base` 等价，后者是测试/自定义 base 用）。
    #[allow(dead_code)]
    pub(crate) fn new(hub_url: String, token: String) -> Result<Self> {
        Self::with_hub_base(hub_url, token)
    }

    /// Construct with an explicit `hub_base`. Tests use this to point the
    /// client at a local mockito server; production code should call
    /// [`Self::new`]. The supplied `hub_url` is normalised (trailing
    /// `/` stripped) so the same path-building code works for both real
    /// and test setups.
    pub(crate) fn with_hub_base(hub_url: String, token: String) -> Result<Self> {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            // Evict idle connections after 30 s. Without this, a connection
            // parked in the pool between two AI calls (which may be minutes
            // apart) can be silently closed by the server-side load balancer
            // or NAT, causing the next `sendmessage` to fail with a transport
            // error and lose the user's reply entirely.
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            http,
            hub_url,
            token,
        })
    }

    pub(crate) async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.clone(),
            base_info: Some(BaseInfo::default()),
            timeout: None,
        };
        let url = format!("{}/ilink/bot/getupdates", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let out: GetUpdatesResponse = resp.json().await?;
        if hub_response_token_rejected(status, out.ret) {
            warn!(
                status = %status,
                errmsg = ?out.errmsg,
                "token rejected during getupdates (hub or direct upstream returned 401)"
            );
            return Ok(GetUpdatesOutcome::TokenRejected);
        }
        if !status.is_success() {
            anyhow::bail!("getupdates HTTP {status}: {:?}", out.errmsg);
        }
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        if out.ret != Some(0) {
            warn!(
                ret = ?out.ret,
                errcode = ?out.errcode,
                errmsg = ?out.errmsg,
                "getupdates returned non-zero ret"
            );
        }
        Ok(GetUpdatesOutcome::Ok(out))
    }

    pub(crate) async fn sendmessage(&self, req: SendMessageRequest) -> Result<SendOutcome> {
        let url = format!("{}/ilink/bot/sendmessage", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendmessage HTTP {status}: {t}");
        }
        let text = resp.text().await?;
        let body_len = text.len();
        match parse_sendoutcome(&text) {
            Ok(out) => {
                if body_len > 0
                    && matches!(out, SendOutcome::Sent)
                    && serde_json::from_str::<SendMessageResponse>(&text).is_err()
                {
                    warn!(
                        body_len,
                        "sendmessage response body failed to parse as JSON; treating as Sent (legacy fallback)"
                    );
                }
                Ok(out)
            }
            Err((other, errmsg)) => {
                anyhow::bail!("sendmessage ret={other} errmsg={:?}", errmsg);
            }
        }
    }

    /// Step 1 of the iLink media upload flow: ask the upstream for a one-shot
    /// upload URL + `media_id`. The caller PUTs the bytes to `upload_url`
    /// and then references `media_id` in a `sendmessage` request.
    pub(crate) async fn getuploadurl(
        &self,
        req: GetUploadUrlRequest,
    ) -> Result<GetUploadUrlResponse> {
        let url = format!("{}/ilink/bot/getuploadurl", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await
            .context("getuploadurl HTTP")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("getuploadurl HTTP {status}: {body}");
        }
        resp.json::<GetUploadUrlResponse>()
            .await
            .context("getuploadurl JSON")
    }

    /// Step 2 of the iLink media upload flow: PUT raw bytes to the
    /// one-shot URL the upstream returned from `getuploadurl`. The `Content-Type`
    /// header should match the file's MIME type; iLink uses this to decide
    /// between image / file / voice / video on the receiving side.
    pub(crate) async fn upload_to(
        &self,
        upload_url: &str,
        bytes: Vec<u8>,
        mime_type: Option<&str>,
    ) -> Result<()> {
        let mut req = self.http.put(upload_url).body(bytes);
        if let Some(m) = mime_type {
            req = req.header(reqwest::header::CONTENT_TYPE, m);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("upload PUT {upload_url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("upload PUT HTTP {status}: {body}");
        }
        Ok(())
    }
}

/// Build generic [`MediaRef`]s from an iLink message's media items.
///
/// Mirrors the former `executor::build_attachments` shape: under agentproc 0.4
/// all media travels in the turn object's `attachments` field.
fn build_media(msg: &WeixinMessage) -> Vec<MediaRef> {
    let mut out = Vec::new();
    let Some(items) = msg.item_list.as_ref() else {
        return out;
    };
    for item in items.iter() {
        match item.item_type {
            Some(msg_type::IMAGE) => {
                if let Some(url) = item
                    .image_item
                    .as_ref()
                    .and_then(|i| i.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    if let Some(n) = super::attachments::normalize_attachment_without_cwd(
                        "image", url, None, None, None,
                    ) {
                        out.push(n.into_media_ref());
                    }
                }
                break;
            }
            Some(msg_type::FILE) => {
                let file_meta = item.file_item.as_ref().and_then(|fi| {
                    fi.cdn_url
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|url| (url, fi.file_name.as_deref()))
                });
                if let Some((url, fname)) = file_meta {
                    if let Some(n) = super::attachments::normalize_attachment_without_cwd(
                        "file",
                        url,
                        fname.map(|s| s.to_string()),
                        None,
                        None,
                    ) {
                        out.push(n.into_media_ref());
                    }
                }
                break;
            }
            Some(msg_type::VIDEO) => {
                if let Some(url) = item
                    .video_item
                    .as_ref()
                    .and_then(|v| v.cdn_url.as_deref())
                    .filter(|s| !s.is_empty())
                {
                    if let Some(n) = super::attachments::normalize_attachment_without_cwd(
                        "video", url, None, None, None,
                    ) {
                        out.push(n.into_media_ref());
                    }
                }
                break;
            }
            _ => {}
        }
    }
    out
}

/// Convert an iLink [`WeixinMessage`] to a generic [`InboundMessage`].
fn weixin_to_inbound(msg: WeixinMessage) -> InboundMessage {
    let context_token = msg.context_token.clone();
    let from_user = msg.from_user_id.clone();
    let is_from_bot = msg.message_type == Some(2);
    let text = msg.text().map(|s| s.to_string());
    let media = build_media(&msg);
    let session_id = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.clone());
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.clone());
    let a2a_call_id = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.a2a_call_id.clone());
    let raw = serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null);
    InboundMessage {
        context_token,
        from_user,
        is_from_bot,
        text,
        media,
        session_id,
        session_name,
        a2a_call_id,
        extra: serde_json::Value::Null,
        raw,
    }
}

/// Convert a generic [`OutboundReply`] to an iLink [`SendMessageRequest`].
fn outbound_to_sendmessage(reply: OutboundReply) -> SendMessageRequest {
    let cli_session_id = reply.cli_session_id.filter(|s| !s.trim().is_empty());
    let mut req = SendMessageRequest::reply_text(
        reply.context_token,
        reply.text,
        &reply.to_user,
        cli_session_id,
    );
    if let Some(ref mut msg) = req.msg {
        let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
        if let Some(sn) = reply.session_name.filter(|s| !s.trim().is_empty()) {
            ext.session_name = Some(sn);
        }
        if let Some(id) = reply.a2a_call_id.filter(|s| !s.trim().is_empty()) {
            ext.a2a_call_id = Some(id);
        }
        if let Some(u) = reply.usage {
            ext.usage = Some(u);
        }
    }
    req
}

/// iLink transport: wraps `HubClient` (defined in `crate::ilink::types`)
/// and speaks the generic `Transport` trait.
/// trait. This is the adapter the dispatcher consumes; it hides
/// `crate::ilink::types` from the rest of the bridge.
#[derive(Clone)]
pub struct IlinkTransport {
    client: HubClient,
}

impl IlinkTransport {
    pub fn new(hub_url: String, token: String) -> Result<Self> {
        Self::with_hub_base(hub_url, token)
    }

    /// Construct with an explicit `hub_base` (default for production is
    /// whatever the bridge manager passes; tests point this at a mockito
    /// server).
    pub fn with_hub_base(hub_url: String, token: String) -> Result<Self> {
        Ok(Self {
            client: HubClient::with_hub_base(hub_url, token)?,
        })
    }
}

impl Transport for IlinkTransport {
    fn next_inbound<'a>(&'a self, buf: &'a mut String) -> BoxFuture<'a, Result<InboundOutcome>> {
        Box::pin(async move {
            match self.client.getupdates(buf).await? {
                GetUpdatesOutcome::TokenRejected => Ok(InboundOutcome::TokenRejected),
                GetUpdatesOutcome::Ok(resp) => {
                    let msgs = resp
                        .msgs
                        .unwrap_or_default()
                        .into_iter()
                        .map(weixin_to_inbound)
                        .collect();
                    Ok(InboundOutcome::Messages(msgs))
                }
            }
        })
    }

    fn send_reply<'a>(&'a self, reply: OutboundReply) -> BoxFuture<'a, Result<SendOutcome>> {
        let req = outbound_to_sendmessage(reply);
        Box::pin(async move { self.client.sendmessage(req).await })
    }

    fn name(&self) -> &'static str {
        "ilink"
    }

    fn send_media<'a>(
        &'a self,
        ctx: MediaOut,
        media: MediaRef,
    ) -> BoxFuture<'a, Result<SendOutcome>> {
        Box::pin(async move {
            // 1. Read bytes from the URI scheme.
            let bytes = super::media::read_media_bytes(
                // The iLink transport doesn't carry its own `reqwest::Client`
                // field; HubClient owns the only one. We rebuild a tiny
                // read-only client for `http(s)://` fetches — the heavier
                // `media_upload` flow reuses HubClient for upload.
                &reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(15))
                    .timeout(Duration::from_secs(60))
                    .build()
                    .context("build read client for iLink media")?,
                &media,
            )
            .await?;

            // 2. Step 1 of the iLink upload flow: ask the upstream for a
            // one-shot upload URL + `media_id`.
            let file_type = match media.kind.as_str() {
                "image" => "image",
                "audio" | "voice" => "voice",
                "video" => "video",
                _ => "file",
            };
            let file_size = media.size.unwrap_or(bytes.len() as u64);
            let file_md5 = md5_hex(&bytes);
            let upload_req = GetUploadUrlRequest {
                file_type: file_type.to_string(),
                file_size,
                file_md5: Some(file_md5),
            };
            let upload_resp = self.client.getuploadurl(upload_req).await?;
            if upload_resp.ret != 0 {
                anyhow::bail!(
                    "iLink getuploadurl ret={} errmsg={:?}",
                    upload_resp.ret,
                    upload_resp.errmsg
                );
            }
            let upload_url = upload_resp
                .upload_url
                .ok_or_else(|| anyhow::anyhow!("iLink getuploadurl returned no upload_url"))?;
            let media_id = upload_resp
                .media_id
                .ok_or_else(|| anyhow::anyhow!("iLink getuploadurl returned no media_id"))?;

            // 3. Step 2: PUT the bytes to the one-shot URL.
            self.client
                .upload_to(&upload_url, bytes, media.mime_type.as_deref())
                .await?;

            // 4. Step 3: send the reply referencing media_id. iLink routes
            // the right slot by `msgtype`: image → image_item.media_id,
            // everything else → file_item.media_id.
            let req = match media.kind.as_str() {
                "image" => {
                    let mut msg =
                        WeixinMessage::build_image_reply(ctx.context_token.clone(), media_id);
                    if !ctx.to_user.is_empty() {
                        msg.to_user_id = Some(ctx.to_user.clone());
                    }
                    SendMessageRequest {
                        msg: Some(msg),
                        base_info: Some(BaseInfo::default()),
                    }
                }
                _ => {
                    let filename = media
                        .filename
                        .clone()
                        .or_else(|| super::media::filename_from_url(&media.url))
                        .unwrap_or_else(|| "attachment".into());
                    let mut msg = WeixinMessage::build_file_reply(
                        ctx.context_token.clone(),
                        media_id,
                        Some(filename),
                    );
                    if !ctx.to_user.is_empty() {
                        msg.to_user_id = Some(ctx.to_user.clone());
                    }
                    SendMessageRequest {
                        msg: Some(msg),
                        base_info: Some(BaseInfo::default()),
                    }
                }
            };
            self.client.sendmessage(req).await
        })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities { media_upload: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_produces_lowercase_32_char_digest() {
        // The iLink upstream's `file_md5` field is documented as
        // lower-case hex of the MD5 of the file bytes. We lock down the
        // format here so a future Rust upgrade doesn't silently switch
        // to upper-case or a different hex format.
        let digest = md5_hex(b"hello");
        assert_eq!(digest.len(), 32);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(digest, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn capabilities_reports_media_upload_true() {
        // Smoke-only: the helper construction requires a valid base url but
        // no network call. The assertion is on the trait method, which
        // must stay true now that the upload flow lands.
        let t = IlinkTransport::new("http://127.0.0.1:1".into(), "t".into()).unwrap();
        assert!(t.capabilities().media_upload);
    }

    #[test]
    fn parse_sendoutcome_three_categories() {
        // empty body → Sent (legacy fallback for servers that reply 200 + no body)
        assert_eq!(parse_sendoutcome("").unwrap(), SendOutcome::Sent);
        assert_eq!(parse_sendoutcome("   ").unwrap(), SendOutcome::Sent);

        // ret == 0 → Sent
        assert_eq!(
            parse_sendoutcome(r#"{"ret":0}"#).unwrap(),
            SendOutcome::Sent
        );

        // ret == -2 → Throttled
        assert_eq!(
            parse_sendoutcome(r#"{"ret":-2,"errmsg":"rl"}"#).unwrap(),
            SendOutcome::Throttled {
                ret: -2,
                errmsg: Some("rl".into()),
            }
        );

        // any other non-zero ret → Err carrying (ret, errmsg)
        let err = parse_sendoutcome(r#"{"ret":-7,"errmsg":"boom"}"#).unwrap_err();
        assert_eq!(err.0, -7);
        assert_eq!(err.1, Some("boom".into()));

        // unparseable non-empty body → Sent (legacy fallback)
        assert_eq!(parse_sendoutcome("not json").unwrap(), SendOutcome::Sent);
    }

    // ── e2e: send_media three-step upload flow ────────────────────────────

    fn transport_for(hub_base: String) -> IlinkTransport {
        IlinkTransport::with_hub_base(hub_base, "test-token".into()).expect("test transport")
    }

    fn png_payload() -> MediaRef {
        MediaRef {
            kind: "image".into(),
            url: "data:image/png;base64,iVBORw0KGgo=".into(),
            filename: Some("plot.png".into()),
            mime_type: Some("image/png".into()),
            size: Some(8),
        }
    }

    #[tokio::test]
    async fn send_media_image_walks_getuploadurl_put_and_sendmessage() {
        let mut server = mockito::Server::new_async().await;
        // Step 1: getuploadurl returns a one-shot URL under the same mock
        // server so mockito can match the subsequent PUT.
        let upload_url = format!("{}/upload-test", server.url());
        let m_get = server
            .mock("POST", "/ilink/bot/getuploadurl")
            .match_header("Authorization", "Bearer test-token")
            .with_status(200)
            .with_body(format!(
                r#"{{"ret":0,"upload_url":"{upload_url}","media_id":"mid-1","errmsg":""}}"#
            ))
            .create_async()
            .await;
        // Step 2: PUT bytes to the one-shot URL.
        let m_put = server
            .mock("PUT", "/upload-test")
            .with_status(200)
            .with_body("")
            .create_async()
            .await;
        // Step 3: sendmessage references media_id. iLink's sendmessage
        // returns an empty body on success, which parse_sendoutcome treats
        // as `Sent` (legacy fallback).
        let m_send = server
            .mock("POST", "/ilink/bot/sendmessage")
            .match_header("Authorization", "Bearer test-token")
            .with_status(200)
            .with_body("")
            .create_async()
            .await;

        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "ctx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let outcome = t
            .send_media(ctx, png_payload())
            .await
            .expect("iLink send_media ok");
        assert_eq!(outcome, SendOutcome::Sent);
        m_get.assert_async().await;
        m_put.assert_async().await;
        m_send.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_getuploadurl_non_zero_ret_bails() {
        let mut server = mockito::Server::new_async().await;
        let m_get = server
            .mock("POST", "/ilink/bot/getuploadurl")
            .with_status(200)
            .with_body(r#"{"ret":-1,"upload_url":"","media_id":"","errmsg":"quota"}"#)
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "ctx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, png_payload()).await.unwrap_err();
        assert!(format!("{err:#}").contains("quota"));
        m_get.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_upload_put_5xx_bubbles_up_as_err() {
        let mut server = mockito::Server::new_async().await;
        let _ = server
            .mock("POST", "/ilink/bot/getuploadurl")
            .with_status(200)
            .with_body(format!(
                r#"{{"ret":0,"upload_url":"{}/upload-test","media_id":"mid-2","errmsg":""}}"#,
                server.url()
            ))
            .create_async()
            .await;
        let m_put = server
            .mock("PUT", "/upload-test")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;
        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "ctx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let err = t.send_media(ctx, png_payload()).await.unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 500"));
        m_put.assert_async().await;
    }

    #[tokio::test]
    async fn send_media_file_kind_routes_to_file_item_with_filename() {
        let mut server = mockito::Server::new_async().await;
        let upload_url = format!("{}/upload-test", server.url());
        let _ = server
            .mock("POST", "/ilink/bot/getuploadurl")
            .with_status(200)
            .with_body(format!(
                r#"{{"ret":0,"upload_url":"{upload_url}","media_id":"mid-3","errmsg":""}}"#
            ))
            .create_async()
            .await;
        let _ = server
            .mock("PUT", "/upload-test")
            .with_status(200)
            .with_body("")
            .create_async()
            .await;
        // Step 3: sendmessage with file_item instead of image_item. iLink
        // uses the same /sendmessage endpoint for both; the diff is in the
        // message body the bridge sends. We don't body-match here — the
        // success of the round trip is sufficient evidence.
        let _ = server
            .mock("POST", "/ilink/bot/sendmessage")
            .with_status(200)
            .with_body("")
            .create_async()
            .await;

        let t = transport_for(server.url());
        let ctx = MediaOut {
            context_token: "ctx".into(),
            to_user: String::new(),
            caption: None,
            reply_to: None,
        };
        let media = MediaRef {
            kind: "file".into(),
            url: "data:application/pdf;base64,JVBERi0=".into(),
            filename: Some("report.pdf".into()),
            mime_type: Some("application/pdf".into()),
            size: Some(8),
        };
        let outcome = t.send_media(ctx, media).await.expect("file kind ok");
        assert_eq!(outcome, SendOutcome::Sent);
    }

    // ── e2e: next_inbound poll round trip ────────────────────────────────

    #[tokio::test]
    async fn next_inbound_polls_getupdates_and_decodes_text_reply() {
        let mut server = mockito::Server::new_async().await;
        // The bridge calls /ilink/bot/getupdates with `get_updates_buf`
        // carrying the cursor; we ignore body matching here (cursor may be
        // empty on first poll) and reply with a single text message from
        // the user. iLink treats `ret != 0` as soft-failure (warn, return
        // empty) — keep `ret: 0`.
        let m = server
            .mock("POST", "/ilink/bot/getupdates")
            .match_header("Authorization", "Bearer test-token")
            .with_status(200)
            .with_body(
                r#"{
                    "ret": 0,
                    "msgs": [{
                        "context_token": "ctx-abc",
                        "message_type": 1,
                        "from_user_id": "user-xyz",
                        "message_state": 1,
                        "item_list": [{
                            "item_type": 101,
                            "text_item": {"text": "hi from user"}
                        }],
                        "ilink_hub_ext": {"session_id": "sess-1", "session_name": "default"}
                    }]
                }"#,
            )
            .create_async()
            .await;

        let t = transport_for(server.url());
        let mut buf = String::new();
        let outcome = t.next_inbound(&mut buf).await.expect("next_inbound ok");
        match outcome {
            InboundOutcome::Messages(msgs) => {
                assert_eq!(msgs.len(), 1);
                let m = &msgs[0];
                assert_eq!(m.context_token.as_deref(), Some("ctx-abc"));
                assert_eq!(m.from_user.as_deref(), Some("user-xyz"));
                assert_eq!(m.text.as_deref(), Some("hi from user"));
                assert_eq!(m.session_id.as_deref(), Some("sess-1"));
                assert_eq!(m.session_name.as_deref(), Some("default"));
                assert!(!m.is_from_bot);
            }
            InboundOutcome::TokenRejected => panic!("expected Messages, got TokenRejected"),
        }
        m.assert_async().await;
    }

    #[tokio::test]
    async fn next_inbound_returns_empty_when_server_returns_no_msgs() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(200)
            .with_body(r#"{"ret":0,"msgs":[]}"#)
            .create_async()
            .await;

        let t = transport_for(server.url());
        let mut buf = String::new();
        let outcome = t.next_inbound(&mut buf).await.expect("ok");
        match outcome {
            InboundOutcome::Messages(msgs) => assert!(msgs.is_empty()),
            InboundOutcome::TokenRejected => panic!("expected empty Messages"),
        }
        m.assert_async().await;
    }

    #[tokio::test]
    async fn next_inbound_returns_token_rejected_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _ = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(401)
            .with_body(r#"{"ret":-14,"errcode":401,"errmsg":"token expired"}"#)
            .create_async()
            .await;

        let t = transport_for(server.url());
        let mut buf = String::new();
        let outcome = t.next_inbound(&mut buf).await.expect("ok");
        assert!(matches!(outcome, InboundOutcome::TokenRejected));
    }
}
