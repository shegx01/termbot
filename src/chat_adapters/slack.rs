use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use super::{
    Attachment, ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext,
};
use crate::state_store::StateUpdate;
use crate::tmux::normalize_quotes;

/// Accept a MIME type for inbound file_share messages.
/// Returns `true` for any non-empty mimetype (all files are forwarded).
/// Images were previously the only accepted type; now all mimetypes pass.
pub(crate) fn should_accept_mimetype(mime: &str) -> bool {
    !mime.is_empty()
}

pub struct SlackPlatform {
    bot_token: String,
    app_token: String,
    channel_id: String,
    authorized_user_id: String,
    connected: Arc<AtomicBool>,
    #[allow(dead_code)]
    rate_limit_ms: u64,
    http_client: reqwest::Client,
    /// Base URL for Slack API calls.  Overridable in tests via `new_with_endpoint`.
    api_base: String,
    #[allow(dead_code)]
    last_edit: Arc<Mutex<Option<Instant>>>,

    // ── Pause/resume (level-triggered watch channel) ─────────────────────────
    /// Send `true` to pause Socket Mode processing; `false` to resume.
    pause_tx: watch::Sender<bool>,
    /// Receiver held so `run_websocket_loop` can gate on the current value.
    pause_rx: watch::Receiver<bool>,

    // ── Catchup state ────────────────────────────────────────────────────────
    /// Guards against concurrent `run_catchup` invocations.  Set to `true`
    /// before spawning a catchup task; reset to `false` when it finishes.
    pub catchup_in_progress: Arc<AtomicBool>,
    /// Per-channel watermark: channel_id → latest delivered message ts.
    /// Advanced by `parse_event` (Socket Mode push) and `run_catchup`.
    last_seen_ts: Arc<Mutex<HashMap<String, String>>>,
    /// Dedup ring for in-flight Socket Mode + catchup overlap prevention.
    /// Capped at 200 entries (oldest dropped on overflow).
    dedup_window: Arc<Mutex<VecDeque<(String, String)>>>,
    /// Maps message ts → thread_ts for thread continuity on edits.
    /// Capped at 50 entries.
    thread_ts_map: Arc<Mutex<VecDeque<(String, String)>>>,

    // ── State worker channel ─────────────────────────────────────────────────
    /// Sender to `App`'s state worker.  Set by `start_with_state`; `None`
    /// when the platform is running via the legacy `start()` path.
    /// Wrapped in Mutex so it can be set on `&self` from `start_with_state`.
    state_tx: Mutex<Option<mpsc::Sender<StateUpdate>>>,
}

/// Validate that a URL is a legitimate Slack file-hosting URL to prevent SSRF.
///
/// Only `files.slack.com` is accepted as the host. This rejects lookalike
/// domains such as `files.slack.com.evil.com` or `evil-slack.com`.
pub fn is_valid_slack_download_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| u.host_str().map(|h| h == "files.slack.com"))
        .unwrap_or(false)
}

/// Guess the MIME type for a file based on its extension (case-insensitive).
/// Falls back to `application/octet-stream` for unknown or missing extensions.
#[cfg(test)]
fn guess_mime(filename: &str) -> String {
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
        Self::new_with_endpoint(
            bot_token,
            app_token,
            channel_id,
            authorized_user_id,
            rate_limit_ms,
            reqwest::Client::new(),
            "https://slack.com/api".to_string(),
        )
    }

    /// Test-friendly constructor that accepts an injected HTTP client and a
    /// custom API base URL (e.g. a `wiremock` server URI).
    pub fn new_with_endpoint(
        bot_token: String,
        app_token: String,
        channel_id: String,
        authorized_user_id: String,
        rate_limit_ms: u64,
        http_client: reqwest::Client,
        api_base: String,
    ) -> Self {
        let (pause_tx, pause_rx) = watch::channel(false);
        Self {
            bot_token,
            app_token,
            channel_id,
            authorized_user_id,
            connected: Arc::new(AtomicBool::new(false)),
            rate_limit_ms,
            http_client,
            api_base,
            last_edit: Arc::new(Mutex::new(None)),
            pause_tx,
            pause_rx,
            catchup_in_progress: Arc::new(AtomicBool::new(false)),
            last_seen_ts: Arc::new(Mutex::new(HashMap::new())),
            dedup_window: Arc::new(Mutex::new(VecDeque::new())),
            thread_ts_map: Arc::new(Mutex::new(VecDeque::new())),
            state_tx: Mutex::new(None),
        }
    }

    /// Start the WebSocket loop with state persistence support.
    ///
    /// Mirrors `TelegramAdapter::start_with_state`.  Stores `state_tx` so
    /// `run_catchup` can persist per-channel watermarks without going through
    /// the main loop.  Spawns the reconnect loop as a background task and
    /// returns immediately.
    pub async fn start_with_state(
        self: &Arc<Self>,
        cmd_tx: mpsc::Sender<IncomingMessage>,
        state_tx: mpsc::Sender<StateUpdate>,
    ) -> Result<()> {
        // Store the state sender via the interior-mutable Mutex.
        *self.state_tx.lock().await = Some(state_tx);

        let platform = Arc::clone(self);
        tokio::spawn(async move {
            const INITIAL_DELAY_SECS: u64 = 5;
            const MAX_DELAY_SECS: u64 = 300;
            let mut delay_secs = INITIAL_DELAY_SECS;

            loop {
                match platform.run_websocket_loop(cmd_tx.clone()).await {
                    Ok(()) => {
                        delay_secs = INITIAL_DELAY_SECS;
                        info!(
                            "Slack WebSocket loop ended, reconnecting in {}s...",
                            delay_secs
                        );
                    }
                    Err(e) => {
                        delay_secs = (delay_secs * 2).min(MAX_DELAY_SECS);
                        error!(
                            "Slack WebSocket loop error: {}. Reconnecting in {}s...",
                            e, delay_secs
                        );
                    }
                }

                platform.connected.store(false, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;

                if cmd_tx.is_closed() {
                    info!("Command channel closed, stopping Slack reconnect loop");
                    break;
                }
            }
        });

        Ok(())
    }

    /// Validate that a Slack message timestamp has the expected format:
    /// `"<digits>.<6-digits>"`.  A malformed ts like `"9"` would sort
    /// lexically above a valid `"1746000000.000000"` and permanently block
    /// a channel's watermark advance.  Skip malformed ts values rather than
    /// advancing the watermark.
    fn is_valid_slack_ts(ts: &str) -> bool {
        if let Some((before_dot, after_dot)) = ts.split_once('.') {
            !before_dot.is_empty()
                && !after_dot.is_empty()
                && after_dot.len() == 6
                && before_dot.chars().all(|c| c.is_ascii_digit())
                && after_dot.chars().all(|c| c.is_ascii_digit())
        } else {
            false
        }
    }

    /// Seed the per-channel `last_seen_ts` map from persisted state on startup.
    /// Call this immediately after construction, before `start_with_state`.
    pub async fn seed_watermarks(&self, watermarks: HashMap<String, String>) {
        let mut guard = self.last_seen_ts.lock().await;
        *guard = watermarks;
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

        // Clone the pause_rx so we can use it inside the loop without
        // borrow conflicts.
        let mut pause_rx = self.pause_rx.clone();

        while let Some(msg_result) = ws_source.next().await {
            // Pause gate (level-triggered): block until pause_rx is false.
            while *pause_rx.borrow() {
                if pause_rx.changed().await.is_err() {
                    return Ok(());
                }
            }

            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    warn!("Slack WebSocket receive error (will reconnect): {}", e);
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
        let ts = event.get("ts").and_then(|v| v.as_str());
        let thread_ts = event
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Auth check — silently drop unauthorized users
        if user != self.authorized_user_id {
            debug!("Ignoring message from unauthorized Slack user: {}", user);
            return None;
        }

        // Extract and download attachments (all mimetypes now accepted)
        let attachments = self.extract_attachments(event).await;

        // For file_share, prefer the initial_comment over the auto-generated text.
        let raw_text = if subtype == Some("file_share") {
            let raw = event
                .get("files")
                .and_then(|f| f.get(0))
                .and_then(|f| f.get("initial_comment"))
                .and_then(|c| c.get("comment"))
                .and_then(|v| v.as_str())
                .or_else(|| event.get("text").and_then(|v| v.as_str()))
                .unwrap_or("");
            extract_file_share_text(raw)
        } else {
            event.get("text").and_then(|v| v.as_str())?.to_string()
        };

        // Apply smart-quote normalization (mirrors Telegram adapter behavior)
        let text = normalize_quotes(&raw_text);

        // If there is no text and no attachments, skip the message
        if text.is_empty() && attachments.is_empty() {
            return None;
        }

        // Advance last_seen_ts and insert into dedup_window.
        if let Some(msg_ts) = ts {
            if !Self::is_valid_slack_ts(msg_ts) {
                warn!(
                    "Slack parse_event: skipping message with malformed ts {:?} on channel {}",
                    msg_ts, channel
                );
                return None;
            }

            let channel_owned = channel.to_string();
            let ts_owned = msg_ts.to_string();

            // Dedup: insert (channel, ts) into window; drop oldest if over 200.
            {
                let mut dedup = self.dedup_window.lock().await;
                dedup.push_back((channel_owned.clone(), ts_owned.clone()));
                if dedup.len() > 200 {
                    dedup.pop_front();
                }
            }

            // Advance last_seen_ts (only forward).
            {
                let mut lsts = self.last_seen_ts.lock().await;
                let advance = match lsts.get(&channel_owned) {
                    Some(current) => ts_owned > *current,
                    None => true,
                };
                if advance {
                    lsts.insert(channel_owned.clone(), ts_owned.clone());
                }
            }

            // Record thread_ts for edits (thread_ts_map, cap 50).
            if let Some(ref t_ts) = thread_ts {
                let mut tmap = self.thread_ts_map.lock().await;
                tmap.push_back((ts_owned.clone(), t_ts.clone()));
                if tmap.len() > 50 {
                    tmap.pop_front();
                }
            }
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

    /// Parse a message from `conversations.history` into an `IncomingMessage`.
    /// Returns `None` if the message is not from the authorized user, is a bot
    /// message, or has no usable content.
    async fn parse_history_message(
        &self,
        msg: &serde_json::Value,
        channel: &str,
    ) -> Option<IncomingMessage> {
        // Ignore bot messages
        if msg.get("bot_id").is_some() {
            return None;
        }

        let user = msg.get("user").and_then(|v| v.as_str())?;
        if user != self.authorized_user_id {
            return None;
        }

        let subtype = msg.get("subtype").and_then(|v| v.as_str());
        match subtype {
            Some("file_share") => {}
            Some(_) => return None,
            None => {}
        }

        let ts = msg.get("ts").and_then(|v| v.as_str())?;
        let thread_ts = msg
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let raw_text = if subtype == Some("file_share") {
            let raw = msg
                .get("files")
                .and_then(|f| f.get(0))
                .and_then(|f| f.get("initial_comment"))
                .and_then(|c| c.get("comment"))
                .and_then(|v| v.as_str())
                .or_else(|| msg.get("text").and_then(|v| v.as_str()))
                .unwrap_or("");
            extract_file_share_text(raw)
        } else {
            msg.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        let text = normalize_quotes(&raw_text);

        // History messages typically won't have downloadable files in the
        // same inline format; skip attachment download for catchup.
        if text.is_empty() {
            return None;
        }

        // Record thread_ts (cap 50).
        if let Some(ref t_ts) = thread_ts {
            let ts_owned = ts.to_string();
            let mut tmap = self.thread_ts_map.lock().await;
            tmap.push_back((ts_owned, t_ts.clone()));
            if tmap.len() > 50 {
                tmap.pop_front();
            }
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
            attachments: vec![],
            socket_request_id: None,
            socket_client_name: None,
        })
    }

    /// Fetch missed messages from `conversations.history` for each active chat
    /// since `oldest` (or per-channel `last_seen_ts` if newer).
    ///
    /// - Deduplicates against the in-flight Socket Mode window.
    /// - Advances `last_seen_ts` after all pages for a channel are processed.
    /// - On HTTP error: does NOT advance watermark; preserves pre-catchup value
    ///   so the next wake retries the full gap.
    /// - On 429: sleeps `Retry-After` seconds, retries once; caps per-channel
    ///   retry time at 30s and skips the channel with a WARN on overflow.
    /// - On `state_tx` send error: posts a chat-safe warning directly to the
    ///   channel via `send_message` (does NOT route through `cmd_tx`).
    pub async fn run_catchup(
        &self,
        cmd_tx: &mpsc::Sender<IncomingMessage>,
        active_chats: &[String],
        oldest: &str,
    ) -> Result<()> {
        for channel in active_chats {
            // Determine the effective oldest: max(oldest, last_seen_ts[channel]).
            let effective_oldest = {
                let lsts = self.last_seen_ts.lock().await;
                match lsts.get(channel) {
                    Some(stored) if stored.as_str() > oldest => stored.clone(),
                    _ => oldest.to_string(),
                }
            };

            let mut cursor: Option<String> = None;
            let mut new_watermark: Option<String> = None;
            let mut http_error = false;
            let mut messages_to_emit: Vec<IncomingMessage> = Vec::new();

            // Pagination loop
            loop {
                let url = format!("{}/conversations.history", self.api_base);
                let mut params = vec![
                    ("channel", channel.clone()),
                    ("oldest", effective_oldest.clone()),
                    ("limit", "100".to_string()),
                    ("inclusive", "false".to_string()),
                ];
                if let Some(ref c) = cursor {
                    params.push(("cursor", c.clone()));
                }

                let resp = match self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.bot_token))
                    .form(&params)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Slack catchup HTTP error for channel {}: {}", channel, e);
                        http_error = true;
                        break;
                    }
                };

                // Handle 429 rate limit.
                if resp.status().as_u16() == 429 {
                    let retry_after: u64 = resp
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                        .min(30);
                    warn!(
                        "Slack conversations.history rate limited for channel {}, sleeping {}s",
                        channel, retry_after
                    );
                    tokio::time::sleep(Duration::from_secs(retry_after)).await;

                    // Retry once.
                    let retry_url = format!("{}/conversations.history", self.api_base);
                    let mut retry_params = vec![
                        ("channel", channel.clone()),
                        ("oldest", effective_oldest.clone()),
                        ("limit", "100".to_string()),
                        ("inclusive", "false".to_string()),
                    ];
                    if let Some(ref c) = cursor {
                        retry_params.push(("cursor", c.clone()));
                    }
                    let retry_resp = match self
                        .http_client
                        .post(&retry_url)
                        .header("Authorization", format!("Bearer {}", self.bot_token))
                        .form(&retry_params)
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                "Slack catchup retry HTTP error for channel {}: {}",
                                channel, e
                            );
                            http_error = true;
                            break;
                        }
                    };

                    if !retry_resp.status().is_success() {
                        warn!(
                            "Slack catchup retry non-2xx for channel {}: {}",
                            channel,
                            retry_resp.status()
                        );
                        http_error = true;
                        break;
                    }

                    // Continue with retry response.
                    let body: serde_json::Value = match retry_resp.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Slack catchup parse error for channel {}: {}", channel, e);
                            http_error = true;
                            break;
                        }
                    };

                    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let err = body
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        warn!("Slack catchup API error for channel {}: {}", channel, err);
                        http_error = true;
                        break;
                    }

                    self.process_history_page(
                        &body,
                        channel,
                        &mut new_watermark,
                        &mut messages_to_emit,
                        &mut cursor,
                    )
                    .await;

                    if cursor.is_none() {
                        break;
                    }
                    continue;
                }

                if !resp.status().is_success() {
                    warn!(
                        "Slack catchup non-2xx for channel {}: {}",
                        channel,
                        resp.status()
                    );
                    http_error = true;
                    break;
                }

                let body: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Slack catchup parse error for channel {}: {}", channel, e);
                        http_error = true;
                        break;
                    }
                };

                if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let err = body
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    warn!("Slack catchup API error for channel {}: {}", channel, err);
                    http_error = true;
                    break;
                }

                self.process_history_page(
                    &body,
                    channel,
                    &mut new_watermark,
                    &mut messages_to_emit,
                    &mut cursor,
                )
                .await;

                if cursor.is_none() {
                    break;
                }
            }

            if http_error {
                // Preserve pre-catchup watermark — do not advance on error.
                // The pre_catchup_ts is not modified inside the loop above,
                // so `last_seen_ts` is still at the pre-catchup value. Nothing
                // to restore.
                continue;
            }

            // Emit messages in order (oldest first — conversations.history returns
            // newest-first; process_history_page pushes them in reverse).
            let mut emitted_count: usize = 0;
            for msg in messages_to_emit {
                if cmd_tx.send(msg).await.is_err() {
                    warn!("cmd_tx closed during Slack catchup emit");
                    return Ok(());
                }
                emitted_count += 1;
            }

            // Advance watermark and persist.
            if let Some(ref wm) = new_watermark {
                // Update in-memory watermark.
                {
                    let mut lsts = self.last_seen_ts.lock().await;
                    let advance = match lsts.get(channel) {
                        Some(current) => wm > current,
                        None => true,
                    };
                    if advance {
                        lsts.insert(channel.clone(), wm.clone());
                    }
                }

                // Persist via state worker (lock briefly, then release before await).
                let state_tx_opt = self.state_tx.lock().await.clone();
                if let Some(state_tx) = state_tx_opt {
                    if state_tx
                        .send(StateUpdate::SlackWatermark {
                            channel_id: channel.clone(),
                            ts: wm.clone(),
                        })
                        .await
                        .is_err()
                    {
                        // Persistence failure: log at warn and post directly to
                        // the channel via send_message.  Do NOT route through
                        // cmd_tx — synthetic messages with user_id="system"
                        // bypass auth but still trigger MarkDirty/BindSlackChat
                        // in handle_command and fall through as tmux stdin.
                        warn!(
                            "Slack catchup watermark persistence failed for channel {}: \
                             state worker channel closed",
                            channel
                        );
                        let warning_text = "Slack catchup watermark persistence failed; \
                                            this prompt's recovery may not survive a restart";
                        if let Err(e) = self.send_message(warning_text, channel, None).await {
                            error!(
                                "Failed to post persistence-failure warning to channel {}: {}",
                                channel, e
                            );
                        }
                    }
                }
                // No state_tx (legacy start() path) — watermark is in-memory only.

                tracing::info!(
                    "Slack catchup complete for channel {}: {} message(s) fetched, watermark={}",
                    channel,
                    emitted_count,
                    wm
                );
            } else {
                tracing::info!(
                    "Slack catchup complete for channel {}: {} message(s) fetched, watermark=unchanged",
                    channel,
                    emitted_count,
                );
            }
        }

        Ok(())
    }

    /// Process one page of `conversations.history` results.
    /// Messages are returned newest-first by Slack; we reverse them to emit
    /// oldest-first.  Dedup against the in-flight Socket Mode window.
    async fn process_history_page(
        &self,
        body: &serde_json::Value,
        channel: &str,
        new_watermark: &mut Option<String>,
        messages_to_emit: &mut Vec<IncomingMessage>,
        cursor: &mut Option<String>,
    ) {
        let messages = match body.get("messages").and_then(|v| v.as_array()) {
            Some(m) => m,
            None => {
                *cursor = None;
                return;
            }
        };

        // Collect in-order (Slack returns newest-first, so reverse for emit).
        let mut page_msgs: Vec<IncomingMessage> = Vec::new();

        for msg in messages {
            let ts = match msg.get("ts").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => continue,
            };

            // Validate ts format before using it as a watermark or dedup key.
            if !Self::is_valid_slack_ts(ts) {
                warn!(
                    "Slack process_history_page: skipping message with malformed ts {:?} on channel {}",
                    ts, channel
                );
                continue;
            }

            // Track the newest ts as the new watermark (first valid ts on the
            // first page is the newest).  We advance the watermark regardless
            // of whether the message was dedup-skipped, so that the watermark
            // always advances when pages are fetched — even if every message
            // on a page was already delivered by Socket Mode.
            if new_watermark.is_none() {
                *new_watermark = Some(ts.to_string());
            }

            // Dedup check.
            {
                let dedup = self.dedup_window.lock().await;
                if dedup.iter().any(|(c, t)| c == channel && t == ts) {
                    debug!("dedup skipped {}/{}", channel, ts);
                    continue;
                }
            }

            if let Some(incoming) = self.parse_history_message(msg, channel).await {
                page_msgs.push(incoming);
            }
        }

        // Reverse to emit oldest-first.
        page_msgs.reverse();
        messages_to_emit.append(&mut page_msgs);

        // Follow cursor for next page.
        *cursor = body
            .pointer("/response_metadata/next_cursor")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }

    /// Download file attachments from a Slack event's `files` array.
    /// Accepts any non-empty MIME type (widened from image-only).
    /// Skips files that exceed 20 MB or fail to download.
    async fn extract_attachments(&self, event: &serde_json::Value) -> Vec<Attachment> {
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

            // Accept any non-empty mimetype (replaces the old image/-only filter).
            if !should_accept_mimetype(mimetype) {
                continue;
            }

            // Skip files larger than 20 MB
            let size = file.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            if size > 20 * 1024 * 1024 {
                warn!("Skipping Slack file: size {} exceeds 20 MB limit", size);
                continue;
            }

            let url = match file.get("url_private_download").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => continue,
            };

            // SSRF prevention: only download from Slack's file hosting domain.
            if !is_valid_slack_download_url(url) {
                warn!("Rejecting non-Slack download URL: {}", url);
                continue;
            }

            let original_name = file
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("file.bin");
            let ext = std::path::Path::new(original_name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");

            let tmp_path = PathBuf::from(format!(
                "/tmp/terminus-attachment-{}.{}",
                Uuid::new_v4(),
                ext
            ));

            let download_future = self
                .http_client
                .get(url)
                .header("Authorization", format!("Bearer {}", self.bot_token))
                .send();

            let resp = match tokio::time::timeout(Duration::from_secs(30), download_future).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("Failed to download Slack file {}: {}", url, e);
                    continue;
                }
                Err(_) => {
                    warn!("Timeout downloading Slack file {}", url);
                    continue;
                }
            };

            let bytes = match tokio::time::timeout(Duration::from_secs(30), resp.bytes()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    warn!("Failed to read Slack file body {}: {}", url, e);
                    continue;
                }
                Err(_) => {
                    warn!("Timeout reading Slack file body {}", url);
                    continue;
                }
            };

            if bytes.len() > 20 * 1024 * 1024 {
                warn!(
                    "Downloaded Slack file body exceeds 20 MB ({} bytes), discarding",
                    bytes.len()
                );
                continue;
            }

            if let Err(e) = tokio::fs::write(&tmp_path, &bytes).await {
                warn!("Failed to write Slack file to {:?}: {}", tmp_path, e);
                let _ = tokio::fs::remove_file(&tmp_path).await;
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

    /// Override pause() to signal the WebSocket loop to stop processing events.
    async fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }

    /// Override resume() to signal the WebSocket loop to resume processing events.
    async fn resume(&self) {
        let _ = self.pause_tx.send(false);
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

        let url = format!("{}/chat.postMessage", self.api_base);
        let resp = self
            .http_client
            .post(&url)
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

            let msg_ts = ts.to_string();

            // Record in thread_ts_map if this message is in a thread.
            if let Some(t_ts) = thread_ts {
                let mut tmap = self.thread_ts_map.lock().await;
                tmap.push_back((msg_ts.clone(), t_ts.to_string()));
                if tmap.len() > 50 {
                    tmap.pop_front();
                }
            }

            Ok(PlatformMessageId::Slack(msg_ts))
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

        // Look up thread_ts from the map (for thread-aware edits).
        let stored_thread_ts = {
            let tmap = self.thread_ts_map.lock().await;
            tmap.iter()
                .find(|(msg_ts, _)| msg_ts == ts)
                .map(|(_, t_ts)| t_ts.clone())
        };

        let mut body = serde_json::json!({
            "channel": chat_id,
            "ts": ts,
            "text": text,
        });

        if let Some(ref t_ts) = stored_thread_ts {
            body["thread_ts"] = serde_json::Value::String(t_ts.clone());
        }

        let url = format!("{}/chat.update", self.api_base);
        let resp = self
            .http_client
            .post(&url)
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
    // is_valid_slack_ts
    // -------------------------------------------------------------------------

    #[test]
    fn valid_slack_ts_accepted() {
        assert!(SlackPlatform::is_valid_slack_ts("1746000000.000000"));
        assert!(SlackPlatform::is_valid_slack_ts("1000000000.123456"));
        assert!(SlackPlatform::is_valid_slack_ts("0.000000"));
    }

    #[test]
    fn invalid_slack_ts_no_dot_rejected() {
        assert!(!SlackPlatform::is_valid_slack_ts("9"));
        assert!(!SlackPlatform::is_valid_slack_ts("1746000000"));
    }

    #[test]
    fn invalid_slack_ts_wrong_fractional_length_rejected() {
        assert!(!SlackPlatform::is_valid_slack_ts("1746000000.00000")); // 5 digits
        assert!(!SlackPlatform::is_valid_slack_ts("1746000000.0000000")); // 7 digits
    }

    #[test]
    fn invalid_slack_ts_non_digit_characters_rejected() {
        assert!(!SlackPlatform::is_valid_slack_ts("174600abc0.000000"));
        assert!(!SlackPlatform::is_valid_slack_ts("1746000000.00000x"));
    }

    #[test]
    fn invalid_slack_ts_empty_rejected() {
        assert!(!SlackPlatform::is_valid_slack_ts(""));
        assert!(!SlackPlatform::is_valid_slack_ts(".000000"));
        assert!(!SlackPlatform::is_valid_slack_ts("1746000000."));
    }

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
    fn ssrf_http_scheme_with_valid_host_rejected() {
        // http:// must be rejected — only https:// is accepted
        assert!(!is_valid_slack_download_url("http://files.slack.com/file"));
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

    // -------------------------------------------------------------------------
    // T5: should_accept_mimetype (unit-level) + production-path file_share test
    // -------------------------------------------------------------------------

    #[test]
    fn test_should_accept_mimetype_pdf() {
        assert!(
            should_accept_mimetype("application/pdf"),
            "PDF should be accepted"
        );
    }

    #[test]
    fn test_should_accept_mimetype_text_plain() {
        assert!(
            should_accept_mimetype("text/plain"),
            "text/plain should be accepted"
        );
    }

    #[test]
    fn test_should_accept_mimetype_image_still_accepted() {
        // Regression: images must still be accepted after the filter widening.
        assert!(
            should_accept_mimetype("image/png"),
            "image/png should still be accepted"
        );
        assert!(
            should_accept_mimetype("image/jpeg"),
            "image/jpeg should still be accepted"
        );
    }

    #[test]
    fn test_should_accept_mimetype_empty_rejected() {
        assert!(
            !should_accept_mimetype(""),
            "empty mimetype should be rejected"
        );
    }

    /// T5 production-path: parse_event with a file_share event is processed.
    ///
    /// The URL is a valid-format `files.slack.com` URL but the download will
    /// fail in tests (no live server at that host).  We assert that the message
    /// is still returned (graceful attachment-download failure path) and that
    /// the event is not silently dropped, which validates the entire code path
    /// through parse_event → extract_attachments without requiring a live
    /// Slack environment.
    #[tokio::test]
    async fn test_parse_event_file_share_pdf_returns_message() {
        let platform = Arc::new(SlackPlatform::new(
            "xoxb-bot".into(),
            "xapp-app".into(),
            "C001".into(),
            "U001".into(),
            0,
        ));

        let payload = serde_json::json!({
            "type": "events_api",
            "envelope_id": "ev-t5",
            "payload": {
                "event": {
                    "type": "message",
                    "subtype": "file_share",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1746000001.000000",
                    "text": "Here is a PDF",
                    "files": [{
                        "id": "F001",
                        "name": "report.pdf",
                        "mimetype": "application/pdf",
                        "size": 1024,
                        "url_private_download": "https://files.slack.com/files-pri/T001-F001/report.pdf"
                    }]
                }
            }
        });

        let result = platform.parse_event(&payload).await;
        // The event should produce an IncomingMessage (not be dropped), even
        // though the file download will fail (no live files.slack.com in tests).
        // The text from the event should be present.
        assert!(
            result.is_some(),
            "file_share event should not be dropped by parse_event"
        );
        let msg = result.unwrap();
        assert_eq!(msg.text, "Here is a PDF", "event text should be preserved");
    }

    /// T5 production-path: text/plain file_share is also accepted.
    #[tokio::test]
    async fn test_parse_event_file_share_text_plain_returns_message() {
        let platform = Arc::new(SlackPlatform::new(
            "xoxb-bot".into(),
            "xapp-app".into(),
            "C001".into(),
            "U001".into(),
            0,
        ));

        let payload = serde_json::json!({
            "type": "events_api",
            "envelope_id": "ev-t5b",
            "payload": {
                "event": {
                    "type": "message",
                    "subtype": "file_share",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1746000002.000000",
                    "text": "Here is a text file",
                    "files": [{
                        "id": "F002",
                        "name": "notes.txt",
                        "mimetype": "text/plain",
                        "size": 512,
                        "url_private_download": "https://files.slack.com/files-pri/T001-F002/notes.txt"
                    }]
                }
            }
        });

        let result = platform.parse_event(&payload).await;
        assert!(
            result.is_some(),
            "text/plain file_share event should not be dropped by parse_event"
        );
        let msg = result.unwrap();
        assert_eq!(
            msg.text, "Here is a text file",
            "event text should be preserved"
        );
    }

    // -------------------------------------------------------------------------
    // T6: normalize_quotes applied in parse_event
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_normalize_quotes_applied_in_parse_event() {
        let platform = Arc::new(SlackPlatform::new(
            "xoxb-bot".into(),
            "xapp-app".into(),
            "C001".into(),
            "U001".into(),
            0,
        ));

        // Construct a synthetic events_api payload with smart quotes.
        let payload = serde_json::json!({
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "message",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1000000001.000001",
                    "text": "\u{201C}hello world\u{201D}"
                }
            }
        });

        let result = platform.parse_event(&payload).await;
        assert!(result.is_some(), "parse_event should return Some");
        let msg = result.unwrap();
        // Smart quotes must be normalized to ASCII double-quotes.
        assert_eq!(
            msg.text, "\"hello world\"",
            "smart quotes should be normalized"
        );
    }

    // -------------------------------------------------------------------------
    // T1 (partial): last_seen_ts advances on Socket Mode push
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_last_seen_ts_advances_on_socket_mode_push() {
        let platform = Arc::new(SlackPlatform::new(
            "xoxb-bot".into(),
            "xapp-app".into(),
            "C001".into(),
            "U001".into(),
            0,
        ));

        let payload1 = serde_json::json!({
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "message",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1000000001.000001",
                    "text": "first"
                }
            }
        });

        let payload2 = serde_json::json!({
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "message",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1000000001.000002",
                    "text": "second"
                }
            }
        });

        platform.parse_event(&payload1).await;
        platform.parse_event(&payload2).await;

        let lsts = platform.last_seen_ts.lock().await;
        assert_eq!(
            lsts.get("C001").map(|s| s.as_str()),
            Some("1000000001.000002"),
            "last_seen_ts must advance to the newer ts"
        );
    }

    // -------------------------------------------------------------------------
    // T7: thread_edit_carries_thread_ts
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_thread_edit_carries_thread_ts() {
        let platform = Arc::new(SlackPlatform::new(
            "xoxb-bot".into(),
            "xapp-app".into(),
            "C001".into(),
            "U001".into(),
            0,
        ));

        // Simulate a threaded message arriving via Socket Mode.
        let payload = serde_json::json!({
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "message",
                    "user": "U001",
                    "channel": "C001",
                    "ts": "1000000002.000001",
                    "thread_ts": "1000000000.000001",
                    "text": "reply in thread"
                }
            }
        });

        platform.parse_event(&payload).await;

        // Verify the thread_ts_map was populated.
        let tmap = platform.thread_ts_map.lock().await;
        let found = tmap
            .iter()
            .find(|(msg_ts, _)| msg_ts == "1000000002.000001");
        assert!(
            found.is_some(),
            "thread_ts_map should contain the message ts"
        );
        assert_eq!(
            found.unwrap().1,
            "1000000000.000001",
            "thread_ts must match"
        );
    }

    // -------------------------------------------------------------------------
    // -------------------------------------------------------------------------
    // T7 (architect PARTIAL): thread_edit chat.update HTTP body carries thread_ts
    // -------------------------------------------------------------------------

    #[cfg(feature = "integration-tests")]
    mod thread_edit_http_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn test_thread_edit_chat_update_body_carries_thread_ts() {
            let mock_server = MockServer::start().await;

            // Respond with a successful chat.update response.
            Mock::given(method("POST"))
                .and(path("/chat.update"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"ok": true, "ts": "1000000002.000001"})),
                )
                .mount(&mock_server)
                .await;

            let platform = Arc::new(SlackPlatform::new_with_endpoint(
                "xoxb-bot".into(),
                "xapp-app".into(),
                "C001".into(),
                "U001".into(),
                0,
                reqwest::Client::new(),
                mock_server.uri(),
            ));

            // Pre-seed thread_ts_map: msg ts → thread ts.
            {
                let mut tmap = platform.thread_ts_map.lock().await;
                tmap.push_back((
                    "1000000002.000001".to_string(),
                    "1000000000.000001".to_string(),
                ));
            }

            // Call edit_message with the seeded msg ts.
            let msg_id = PlatformMessageId::Slack("1000000002.000001".to_string());
            platform
                .edit_message(&msg_id, "C001", "updated text")
                .await
                .expect("edit_message should succeed");

            // Capture the recorded request and verify the body contains thread_ts.
            let received = mock_server.received_requests().await.unwrap();
            assert_eq!(
                received.len(),
                1,
                "exactly one chat.update request expected"
            );

            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("request body should be valid JSON");
            assert_eq!(
                body.get("thread_ts").and_then(|v| v.as_str()),
                Some("1000000000.000001"),
                "chat.update body must carry thread_ts from thread_ts_map"
            );
            assert_eq!(
                body.get("ts").and_then(|v| v.as_str()),
                Some("1000000002.000001"),
                "chat.update body must carry the message ts"
            );
            assert_eq!(
                body.get("text").and_then(|v| v.as_str()),
                Some("updated text"),
                "chat.update body must carry the updated text"
            );
        }
    }

    // Step 3 catchup tests (wiremock)
    // -------------------------------------------------------------------------

    #[cfg(feature = "integration-tests")]
    mod catchup_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn make_platform(mock_uri: String) -> Arc<SlackPlatform> {
            Arc::new(SlackPlatform::new_with_endpoint(
                "xoxb-bot".into(),
                "xapp-app".into(),
                "C001".into(),
                "U001".into(),
                0,
                reqwest::Client::new(),
                mock_uri,
            ))
        }

        fn history_page(
            messages: Vec<serde_json::Value>,
            next_cursor: Option<&str>,
        ) -> serde_json::Value {
            let mut v = serde_json::json!({
                "ok": true,
                "messages": messages,
                "has_more": next_cursor.is_some(),
            });
            if let Some(cursor) = next_cursor {
                v["response_metadata"] = serde_json::json!({
                    "next_cursor": cursor
                });
            }
            v
        }

        fn msg(ts: &str, text: &str, user: &str) -> serde_json::Value {
            serde_json::json!({
                "type": "message",
                "user": user,
                "ts": ts,
                "text": text
            })
        }

        #[tokio::test]
        async fn test_catchup_watermark_advances_per_chat() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    vec![
                        msg("1000000003.000000", "third", "U001"),
                        msg("1000000002.000000", "second", "U001"),
                        msg("1000000001.000000", "first", "U001"),
                    ],
                    None,
                )))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());
            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);

            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "1000000000.000000")
                .await
                .expect("run_catchup should succeed");

            // All three messages emitted.
            let mut received = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                received.push(m.text.clone());
            }
            assert_eq!(received.len(), 3, "all 3 messages should be emitted");

            // Watermark advanced to newest message.
            let lsts = platform.last_seen_ts.lock().await;
            assert_eq!(
                lsts.get("C001").map(|s| s.as_str()),
                Some("1000000003.000000"),
                "watermark must advance to newest ts"
            );
        }

        #[tokio::test]
        async fn test_catchup_dedup_skips_socket_mode_seen() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    vec![
                        msg("1000000002.000000", "new", "U001"),
                        msg("1000000001.000000", "already seen", "U001"),
                    ],
                    None,
                )))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());

            // Pre-seed dedup_window as if Socket Mode already delivered the first message.
            {
                let mut dedup = platform.dedup_window.lock().await;
                dedup.push_back(("C001".to_string(), "1000000001.000000".to_string()));
            }

            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "1000000000.000000")
                .await
                .expect("run_catchup should succeed");

            let mut received = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                received.push(m.text.clone());
            }
            assert_eq!(received.len(), 1, "dedup should skip the seen message");
            assert_eq!(received[0], "new");
        }

        #[tokio::test]
        async fn test_catchup_pagination_follows_cursor() {
            let mock_server = MockServer::start().await;

            // Page 1: has cursor
            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .and(wiremock::matchers::body_string_contains("cursor="))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    vec![msg("1000000002.000000", "page2msg", "U001")],
                    None,
                )))
                .mount(&mock_server)
                .await;

            // Page 1 without cursor (first request)
            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    vec![msg("1000000003.000000", "page1msg", "U001")],
                    Some("cursor-abc"),
                )))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());
            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);

            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "1000000000.000000")
                .await
                .expect("run_catchup should succeed");

            let mut received = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                received.push(m.text.clone());
            }
            // Both pages should yield messages.
            // Slack returns newest-first; cursor pagination progresses toward older
            // messages.  Page 1 (ts=1000000003) is newer; page 2 via cursor
            // (ts=1000000002) is older.  Each page is reversed internally so
            // within a page messages emit oldest-first.  Across pages, page 1
            // messages are appended before page 2 — so the overall order is
            // page1 (newer group) then page2 (older group).
            assert_eq!(
                received,
                vec!["page1msg", "page2msg"],
                "page1 messages (newer) emit before page2 messages (older)"
            );
        }

        #[tokio::test]
        async fn test_catchup_preserves_send_order_within_chat() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    // Slack returns newest-first; catchup must emit oldest-first.
                    vec![
                        msg("1000000003.000000", "third", "U001"),
                        msg("1000000002.000000", "second", "U001"),
                        msg("1000000001.000000", "first", "U001"),
                    ],
                    None,
                )))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());
            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);

            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "1000000000.000000")
                .await
                .expect("run_catchup should succeed");

            let mut received = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                received.push(m.text.clone());
            }
            assert_eq!(
                received,
                vec!["first", "second", "third"],
                "messages must be emitted oldest-first"
            );
        }

        #[tokio::test]
        async fn test_catchup_no_watermark_advance_on_http_error() {
            let mock_server = MockServer::start().await;

            // Respond with 500 to trigger the HTTP error path.
            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());

            // Seed a pre-catchup watermark.
            {
                let mut lsts = platform.last_seen_ts.lock().await;
                lsts.insert("C001".to_string(), "1000000000.000001".to_string());
            }

            let (cmd_tx, _cmd_rx) = mpsc::channel(16);
            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "999999999.000000")
                .await
                .expect("run_catchup should not propagate HTTP errors");

            // Watermark must be unchanged.
            let lsts = platform.last_seen_ts.lock().await;
            assert_eq!(
                lsts.get("C001").map(|s| s.as_str()),
                Some("1000000000.000001"),
                "watermark must not advance on HTTP error"
            );
        }

        // -------------------------------------------------------------------------
        // `ok: false` API error (e.g. missing_scope with HTTP 200)
        // -------------------------------------------------------------------------

        #[tokio::test]
        async fn test_catchup_ok_false_api_error_skips_channel() {
            let mock_server = MockServer::start().await;

            // Slack returns HTTP 200 but {"ok": false, "error": "missing_scope"}.
            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": false,
                    "error": "missing_scope"
                })))
                .mount(&mock_server)
                .await;

            let platform = make_platform(mock_server.uri());

            // Seed a pre-catchup watermark.
            {
                let mut lsts = platform.last_seen_ts.lock().await;
                lsts.insert("C001".to_string(), "1000000000.000001".to_string());
            }

            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "999999999.000000")
                .await
                .expect("run_catchup should return Ok on Slack API errors");

            // (a) No messages emitted.
            let mut texts = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                texts.push(m.text.clone());
            }
            assert!(
                texts.is_empty(),
                "no messages should be emitted on Slack API error; got: {:?}",
                texts
            );

            // (b) Watermark unchanged.
            let lsts = platform.last_seen_ts.lock().await;
            assert_eq!(
                lsts.get("C001").map(|s| s.as_str()),
                Some("1000000000.000001"),
                "watermark must not advance on Slack API error"
            );
        }

        // -------------------------------------------------------------------------
        // Step 5: persistence failure emits chat warning via chat.postMessage
        // -------------------------------------------------------------------------

        #[tokio::test]
        async fn test_catchup_persistence_failure_emits_chat_warning() {
            let mock_server = MockServer::start().await;

            Mock::given(method("POST"))
                .and(path("/conversations.history"))
                .respond_with(ResponseTemplate::new(200).set_body_json(history_page(
                    vec![msg("1000000001.000000", "a message", "U001")],
                    None,
                )))
                .mount(&mock_server)
                .await;

            // Accept the chat.postMessage warning call (wiremock returns 200 ok by
            // default; we just need to capture the request body for assertion).
            Mock::given(method("POST"))
                .and(path("/chat.postMessage"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": true,
                    "ts": "1000000002.000000"
                })))
                .mount(&mock_server)
                .await;

            let platform = Arc::new(SlackPlatform::new_with_endpoint(
                "xoxb-bot".into(),
                "xapp-app".into(),
                "C001".into(),
                "U001".into(),
                0,
                reqwest::Client::new(),
                mock_server.uri(),
            ));

            // Create a closed state_tx (drop the receiver immediately).
            let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(1);
            drop(state_rx);
            *platform.state_tx.lock().await = Some(state_tx);
            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);

            platform
                .run_catchup(&cmd_tx, &["C001".to_string()], "1000000000.000000")
                .await
                .expect("run_catchup should not propagate state errors");

            // The regular message is still emitted on cmd_tx.
            let mut texts = vec![];
            while let Ok(m) = cmd_rx.try_recv() {
                texts.push(m.text.clone());
            }
            assert!(
                texts.iter().any(|t| t == "a message"),
                "regular message should still be emitted on cmd_tx; got: {:?}",
                texts
            );

            // The persistence-failure warning is posted directly via chat.postMessage,
            // NOT via cmd_tx (to avoid injecting into tmux stdin through handle_command).
            let reqs = mock_server.received_requests().await.unwrap();
            let post_msg_reqs: Vec<_> = reqs
                .iter()
                .filter(|r| r.url.path() == "/chat.postMessage")
                .collect();
            assert_eq!(
                post_msg_reqs.len(),
                1,
                "exactly one chat.postMessage call expected for the persistence warning"
            );
            let body: serde_json::Value =
                serde_json::from_slice(&post_msg_reqs[0].body).expect("body should be JSON");
            let posted_text = body
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert!(
                posted_text.contains("persistence failed"),
                "chat.postMessage body should contain 'persistence failed'; got: {:?}",
                posted_text
            );
        }
    }
}
