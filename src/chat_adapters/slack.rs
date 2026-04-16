use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use super::{
    Attachment, ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext,
};

pub struct SlackPlatform {
    bot_token: String,
    app_token: String,
    channel_id: String,
    authorized_user_id: String,
    connected: Arc<AtomicBool>,
    #[allow(dead_code)]
    rate_limit_ms: u64,
    http_client: reqwest::Client,
    #[allow(dead_code)]
    last_edit: Arc<Mutex<Option<Instant>>>,
}

/// Validate that a URL is a legitimate Slack file-hosting URL to prevent SSRF.
///
/// Only `files.slack.com` is accepted as the host. This rejects lookalike
/// domains such as `files.slack.com.evil.com` or `evil-slack.com`.
pub fn is_valid_slack_download_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h == "files.slack.com"))
        .unwrap_or(false)
}

/// Guess the MIME type for a file based on its extension (case-insensitive).
/// Falls back to `application/octet-stream` for unknown or missing extensions.
#[allow(dead_code)]
pub fn guess_mime(filename: &str) -> String {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Extract the user-supplied text from a `file_share` event.
///
/// Slack auto-generates text like `"<@U123> uploaded a file: <url|name>"` when
/// a file is uploaded without a caption. Treat any string containing the
/// literal `"uploaded a file:"` as empty so that caption-less photo messages
/// are handled correctly.
pub fn extract_file_share_text(raw: &str) -> String {
    if raw.contains("uploaded a file:") {
        String::new()
    } else {
        raw.to_string()
    }
}

impl SlackPlatform {
    pub fn new(
        bot_token: String,
        app_token: String,
        channel_id: String,
        authorized_user_id: String,
        rate_limit_ms: u64,
    ) -> Self {
        Self {
            bot_token,
            app_token,
            channel_id,
            authorized_user_id,
            connected: Arc::new(AtomicBool::new(false)),
            rate_limit_ms,
            http_client: reqwest::Client::new(),
            last_edit: Arc::new(Mutex::new(None)),
        }
    }

    async fn get_websocket_url(&self) -> Result<String> {
        let resp = self
            .http_client
            .post("https://slack.com/api/apps.connections.open")
            .header("Authorization", format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await
            .context("Failed to call apps.connections.open")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse apps.connections.open response")?;

        if body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let url = body
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("apps.connections.open response missing 'url' field"))?;
            Ok(url.to_string())
        } else {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("apps.connections.open failed: {}", err))
        }
    }

    async fn run_websocket_loop(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let ws_url = self.get_websocket_url().await?;
        info!("Connecting to Slack Socket Mode (url redacted for security)");

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("Failed to connect to Slack WebSocket")?;

        self.connected.store(true, Ordering::SeqCst);
        info!("Slack WebSocket connected");

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        while let Some(msg_result) = ws_source.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("WebSocket receive error: {}", e);
                    break;
                }
            };

            match msg {
                Message::Text(text) => {
                    debug!("Received Slack WS message ({} bytes)", text.len());
                    let payload: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Failed to parse Slack WS JSON: {}", e);
                            continue;
                        }
                    };

                    // Acknowledge with envelope_id if present
                    if let Some(envelope_id) = payload.get("envelope_id").and_then(|v| v.as_str()) {
                        let ack = serde_json::json!({ "envelope_id": envelope_id });
                        if let Err(e) = ws_sink.send(Message::Text(ack.to_string())).await {
                            error!("Failed to send Slack WS ack: {}", e);
                            break;
                        }
                    }

                    // Handle hello (connection confirmation)
                    if payload.get("type").and_then(|v| v.as_str()) == Some("hello") {
                        info!("Slack Socket Mode hello received");
                        continue;
                    }

                    // Handle events_api messages
                    if payload.get("type").and_then(|v| v.as_str()) == Some("events_api") {
                        if let Some(incoming) = self.parse_event(&payload).await {
                            if cmd_tx.send(incoming).await.is_err() {
                                warn!("Command channel closed, stopping Slack listener");
                                break;
                            }
                        }
                    }
                }
                Message::Ping(data) => {
                    if let Err(e) = ws_sink.send(Message::Pong(data)).await {
                        error!("Failed to send WebSocket pong: {}", e);
                        break;
                    }
                }
                Message::Close(_) => {
                    info!("Slack WebSocket closed by server");
                    break;
                }
                _ => {}
            }
        }

        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn parse_event(&self, payload: &serde_json::Value) -> Option<IncomingMessage> {
        let event = payload.get("payload")?.get("event")?;

        if event.get("type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }

        // Ignore bot messages
        if event.get("bot_id").is_some() {
            return None;
        }

        // Allow file_share subtype; block all other subtypes (edits, deletes, joins, etc.)
        let subtype = event.get("subtype").and_then(|v| v.as_str());
        match subtype {
            Some("file_share") => {} // allow user-uploaded files
            Some(_) => return None,  // block edits, deletes, joins, etc.
            None => {}               // normal text messages
        }

        let user = event.get("user").and_then(|v| v.as_str())?;
        let channel = event
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.channel_id);
        let thread_ts = event
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Auth check — silently drop unauthorized users
        if user != self.authorized_user_id {
            debug!("Ignoring message from unauthorized Slack user: {}", user);
            return None;
        }

        // Extract and download image attachments
        let attachments = self.extract_images(event).await;

        // For file_share, prefer the initial_comment over the auto-generated text.
        // Slack auto-generates text like "<@U123> uploaded a file: <url|name>" when
        // there's no user caption — treat that as empty so photo-only messages work.
        let text = if subtype == Some("file_share") {
            let raw = event
                .get("files")
                .and_then(|f| f.get(0))
                .and_then(|f| f.get("initial_comment"))
                .and_then(|c| c.get("comment"))
                .and_then(|v| v.as_str())
                .or_else(|| event.get("text").and_then(|v| v.as_str()))
                .unwrap_or("");
            // Strip Slack's auto-generated upload text
            extract_file_share_text(raw)
        } else {
            event.get("text").and_then(|v| v.as_str())?.to_string()
        };

        // If there is no text and no attachments, skip the message
        if text.is_empty() && attachments.is_empty() {
            return None;
        }

        Some(IncomingMessage {
            user_id: user.to_string(),
            text,
            platform: PlatformType::Slack,
            reply_context: ReplyContext {
                platform: PlatformType::Slack,
                chat_id: channel.to_string(),
                thread_ts,
                socket_reply_tx: None,
            },
            attachments,
            socket_request_id: None,
            socket_client_name: None,
        })
    }

    /// Download image files from a Slack event's `files` array.
    /// Skips files that are not images, exceed 20 MB, or fail to download.
    async fn extract_images(&self, event: &serde_json::Value) -> Vec<Attachment> {
        let files = match event.get("files").and_then(|v| v.as_array()) {
            Some(f) => f,
            None => return vec![],
        };

        let mut attachments = Vec::new();

        for file in files {
            let mimetype = match file.get("mimetype").and_then(|v| v.as_str()) {
                Some(m) => m,
                None => continue,
            };

            if !mimetype.starts_with("image/") {
                continue;
            }

            // Skip files larger than 20 MB
            let size = file.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            if size > 20 * 1024 * 1024 {
                warn!("Skipping Slack image: size {} exceeds 20 MB limit", size);
                continue;
            }

            let url = match file.get("url_private_download").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => continue,
            };

            // SSRF prevention: only download from Slack's file hosting domain.
            // Using exact match to prevent bypasses like evil-slack.com.
            if !is_valid_slack_download_url(url) {
                warn!("Rejecting non-Slack download URL: {}", url);
                continue;
            }

            // Derive file extension from the original filename, defaulting to "jpg"
            let original_name = file
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("image.jpg");
            let ext = std::path::Path::new(original_name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("jpg");

            let tmp_path = PathBuf::from(format!("/tmp/terminus-img-{}.{}", Uuid::new_v4(), ext));

            let download_future = self
                .http_client
                .get(url)
                .header("Authorization", format!("Bearer {}", self.bot_token))
                .send();

            let resp = match tokio::time::timeout(Duration::from_secs(30), download_future).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("Failed to download Slack image {}: {}", url, e);
                    continue;
                }
                Err(_) => {
                    warn!("Timeout downloading Slack image {}", url);
                    continue;
                }
            };

            let bytes = match tokio::time::timeout(Duration::from_secs(30), resp.bytes()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    warn!("Failed to read Slack image body {}: {}", url, e);
                    continue;
                }
                Err(_) => {
                    warn!("Timeout reading Slack image body {}", url);
                    continue;
                }
            };

            // Enforce actual body size limit (Slack metadata `size` field is untrusted)
            if bytes.len() > 20 * 1024 * 1024 {
                warn!(
                    "Downloaded Slack image body exceeds 20 MB ({} bytes), discarding",
                    bytes.len()
                );
                continue;
            }

            if let Err(e) = tokio::fs::write(&tmp_path, &bytes).await {
                warn!("Failed to write Slack image to {:?}: {}", tmp_path, e);
                continue;
            }

            attachments.push(Attachment {
                path: tmp_path,
                filename: original_name.to_string(),
                media_type: mimetype.to_string(),
            });
        }

        attachments
    }

    /// Upload a file to Slack using the v2 three-step flow:
    /// 1. `files.getUploadURLExternal` — obtain upload URL and file_id
    /// 2. PUT raw bytes to the returned upload URL
    /// 3. `files.completeUploadExternal` — finalize and share to channel
    async fn upload_file_v2(
        &self,
        file_bytes: Vec<u8>,
        filename: &str,
        caption: Option<&str>,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        if file_bytes.is_empty() {
            anyhow::bail!("Cannot upload zero-byte file to Slack");
        }

        // Step 1: Get upload URL
        let get_url_resp = self
            .http_client
            .post("https://slack.com/api/files.getUploadURLExternal")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .form(&[
                ("filename", filename.to_string()),
                ("length", file_bytes.len().to_string()),
            ])
            .send()
            .await
            .context("Failed to call files.getUploadURLExternal")?;

        let get_url_json: serde_json::Value = get_url_resp
            .json()
            .await
            .context("Failed to parse files.getUploadURLExternal response")?;

        if !get_url_json["ok"].as_bool().unwrap_or(false) {
            let err = get_url_json["error"].as_str().unwrap_or("unknown error");
            anyhow::bail!("files.getUploadURLExternal failed: {}", err);
        }

        let upload_url = get_url_json["upload_url"]
            .as_str()
            .ok_or_else(|| anyhow!("files.getUploadURLExternal response missing 'upload_url'"))?
            .to_string();
        let file_id = get_url_json["file_id"]
            .as_str()
            .ok_or_else(|| anyhow!("files.getUploadURLExternal response missing 'file_id'"))?
            .to_string();

        // Step 2: Upload file bytes to the provided URL (Slack-supplied, not user input)
        self.http_client
            .put(&upload_url)
            .body(file_bytes)
            .send()
            .await
            .context("Failed to PUT file bytes to Slack upload URL")?
            .error_for_status()
            .context("Slack upload URL returned non-2xx status")?;

        // Step 3: Complete upload and share to channel
        let mut complete_body = serde_json::json!({
            "files": [{"id": file_id, "title": filename}],
            "channel_id": channel_id,
        });

        if let Some(ts) = thread_ts {
            complete_body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }
        if let Some(cap) = caption {
            complete_body["initial_comment"] = serde_json::Value::String(cap.to_string());
        }

        let complete_resp = self
            .http_client
            .post("https://slack.com/api/files.completeUploadExternal")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&complete_body)
            .send()
            .await
            .context("Failed to call files.completeUploadExternal")?;

        let complete_json: serde_json::Value = complete_resp
            .json()
            .await
            .context("Failed to parse files.completeUploadExternal response")?;

        if !complete_json["ok"].as_bool().unwrap_or(false) {
            let err = complete_json["error"].as_str().unwrap_or("unknown error");
            anyhow::bail!("files.completeUploadExternal failed: {}", err);
        }

        // Extract ts from the completed file's share info when available
        let ts = complete_json
            .pointer("/files/0/shares/public")
            .or_else(|| complete_json.pointer("/files/0/shares/private"))
            .and_then(|shares| shares.as_object())
            .and_then(|map| map.values().next())
            .and_then(|arr| arr.get(0))
            .and_then(|share| share.get("ts"))
            .and_then(|v| v.as_str())
            .unwrap_or("uploaded")
            .to_string();

        Ok(PlatformMessageId::Slack(ts))
    }
}

#[async_trait]
impl ChatPlatform for SlackPlatform {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        const INITIAL_DELAY_SECS: u64 = 5;
        const MAX_DELAY_SECS: u64 = 300;

        // NOTE: Slack Socket Mode does not replay unacknowledged events on
        // reconnection (unlike Telegram's getUpdates offset model). Messages
        // sent during the reconnect window are permanently lost. This is an
        // inherent limitation of the Socket Mode transport.
        let mut delay_secs = INITIAL_DELAY_SECS;

        loop {
            match self.run_websocket_loop(cmd_tx.clone()).await {
                Ok(()) => {
                    // Clean disconnect — reset backoff, do NOT double it
                    delay_secs = INITIAL_DELAY_SECS;
                    info!(
                        "Slack WebSocket loop ended, reconnecting in {}s...",
                        delay_secs
                    );
                }
                Err(e) => {
                    // Double the backoff on errors only, not on clean disconnects
                    delay_secs = (delay_secs * 2).min(MAX_DELAY_SECS);
                    error!(
                        "Slack WebSocket loop error: {}. Reconnecting in {}s...",
                        e, delay_secs
                    );
                }
            }

            self.connected.store(false, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;

            // Stop reconnecting if the receiver has been dropped
            if cmd_tx.is_closed() {
                info!("Command channel closed, stopping Slack reconnect loop");
                break;
            }
        }

        Ok(())
    }

    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let mut body = serde_json::json!({
            "channel": chat_id,
            "text": text,
        });

        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let resp = self
            .http_client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .context("Failed to call chat.postMessage")?;

        let result: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse chat.postMessage response")?;

        if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let ts = result
                .get("ts")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("chat.postMessage response missing 'ts' field"))?;
            Ok(PlatformMessageId::Slack(ts.to_string()))
        } else {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("chat.postMessage failed: {}", err))
        }
    }

    async fn edit_message(
        &self,
        msg_id: &PlatformMessageId,
        chat_id: &str,
        text: &str,
    ) -> Result<()> {
        let ts = match msg_id {
            PlatformMessageId::Slack(ts) => ts,
            _ => {
                return Err(anyhow!(
                    "edit_message called with non-Slack PlatformMessageId"
                ))
            }
        };

        // Rate limiting: release lock before sleeping to avoid blocking concurrent calls
        let sleep_dur = {
            let mut last = self.last_edit.lock().await;
            let dur = if let Some(t) = *last {
                let elapsed = t.elapsed();
                let min_gap = Duration::from_millis(self.rate_limit_ms);
                if elapsed < min_gap {
                    Some(min_gap - elapsed)
                } else {
                    None
                }
            } else {
                None
            };
            *last = Some(Instant::now());
            dur
        };
        if let Some(d) = sleep_dur {
            tokio::time::sleep(d).await;
        }

        let body = serde_json::json!({
            "channel": chat_id,
            "ts": ts,
            "text": text,
        });

        let resp = self
            .http_client
            .post("https://slack.com/api/chat.update")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .context("Failed to call chat.update")?;

        let result: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse chat.update response")?;

        if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("chat.update failed: {}", err))
        }
    }

    async fn send_photo(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        self.upload_file_v2(data.to_vec(), filename, caption, chat_id, thread_ts)
            .await
    }

    async fn send_document(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        // Slack's files.upload handles any file type — reuse the same logic as send_photo
        self.send_photo(data, filename, caption, chat_id, thread_ts)
            .await
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn platform_type(&self) -> PlatformType {
        PlatformType::Slack
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // is_valid_slack_download_url
    // -------------------------------------------------------------------------

    #[test]
    fn ssrf_valid_slack_download_url_accepted() {
        assert!(is_valid_slack_download_url(
            "https://files.slack.com/files-pri/T123/download/image.png"
        ));
    }

    #[test]
    fn ssrf_lookalike_evil_slack_domain_rejected() {
        assert!(!is_valid_slack_download_url("https://evil-slack.com/files"));
    }

    #[test]
    fn ssrf_subdomain_confusion_rejected() {
        // files.slack.com.evil.com is NOT files.slack.com
        assert!(!is_valid_slack_download_url(
            "https://files.slack.com.evil.com/exploit"
        ));
    }

    #[test]
    fn ssrf_slack_com_without_files_subdomain_rejected() {
        // slack.com (no files. prefix) must be rejected
        assert!(!is_valid_slack_download_url("https://slack.com/files"));
    }

    #[test]
    fn ssrf_http_scheme_with_valid_host_accepted() {
        // Scheme does not matter — only the host is checked
        assert!(is_valid_slack_download_url("http://files.slack.com/file"));
    }

    #[test]
    fn ssrf_non_url_string_rejected() {
        assert!(!is_valid_slack_download_url("not-a-url"));
    }

    #[test]
    fn ssrf_empty_string_rejected() {
        assert!(!is_valid_slack_download_url(""));
    }

    // -------------------------------------------------------------------------
    // guess_mime
    // -------------------------------------------------------------------------

    #[test]
    fn guess_mime_jpg_lowercase() {
        assert_eq!(guess_mime("photo.jpg"), "image/jpeg");
    }

    #[test]
    fn guess_mime_jpeg_uppercase_case_insensitive() {
        assert_eq!(guess_mime("photo.JPEG"), "image/jpeg");
    }

    #[test]
    fn guess_mime_png() {
        assert_eq!(guess_mime("doc.png"), "image/png");
    }

    #[test]
    fn guess_mime_unknown_extension_falls_back_to_octet_stream() {
        assert_eq!(guess_mime("file.xyz"), "application/octet-stream");
    }

    #[test]
    fn guess_mime_no_extension_falls_back_to_octet_stream() {
        assert_eq!(guess_mime("no_extension"), "application/octet-stream");
    }

    // -------------------------------------------------------------------------
    // extract_file_share_text
    // -------------------------------------------------------------------------

    #[test]
    fn file_share_text_auto_generated_upload_text_stripped() {
        // Slack's auto-generated "<@U123> uploaded a file: <url|name>" becomes empty
        assert_eq!(
            extract_file_share_text("<@U123> uploaded a file: <url|name>"),
            ""
        );
    }

    #[test]
    fn file_share_text_user_caption_preserved() {
        assert_eq!(
            extract_file_share_text("Here is my caption"),
            "Here is my caption"
        );
    }

    #[test]
    fn file_share_text_empty_input_stays_empty() {
        assert_eq!(extract_file_share_text(""), "");
    }

    #[test]
    fn file_share_text_inline_pattern_treated_as_empty() {
        // A plain-language sentence that contains the trigger phrase is also stripped
        assert_eq!(extract_file_share_text("I uploaded a file: report.pdf"), "");
    }
}
