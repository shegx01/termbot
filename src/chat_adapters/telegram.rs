use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use teloxide::net::{default_reqwest_settings, Download};
use teloxide::payloads::GetUpdatesSetters;
use teloxide::prelude::*;
use teloxide::types::{AllowedUpdate, ChatId, InputFile, UpdateKind};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

use super::{
    Attachment, ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext,
};
use crate::state_store::StateUpdate;

/// Compute exponential backoff delay in seconds for polling retries.
/// Starts at 5s, doubles each consecutive failure, capped at 60s.
fn polling_backoff_secs(consecutive_failures: u32) -> u64 {
    std::cmp::min(
        5u64.saturating_mul(2u64.saturating_pow(consecutive_failures.saturating_sub(1))),
        60,
    )
}

pub struct TelegramAdapter {
    bot: teloxide::Bot,
    authorized_user_id: u64,
    connected: Arc<AtomicBool>,
    #[allow(dead_code)]
    rate_limit_ms: u64,
    #[allow(dead_code)]
    last_edit: Arc<tokio::sync::Mutex<Option<Instant>>>,
    /// Telegram getUpdates offset to seed on startup (loaded from persisted state).
    initial_offset: i64,
    /// Watch channel sender: `true` = polling paused, `false` = polling active.
    pause_tx: Arc<tokio::sync::watch::Sender<bool>>,
    /// Receiver clone stored so `start()` can move one into the polling task.
    pause_rx: tokio::sync::watch::Receiver<bool>,
}

impl TelegramAdapter {
    pub fn new(token: String, authorized_user_id: u64, rate_limit_ms: u64) -> Self {
        let client = default_reqwest_settings()
            .timeout(Duration::from_secs(45))
            .build()
            .expect("Failed to build reqwest client");
        let bot = teloxide::Bot::with_client(token, client);
        let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);
        Self {
            bot,
            authorized_user_id,
            connected: Arc::new(AtomicBool::new(false)),
            rate_limit_ms,
            last_edit: Arc::new(tokio::sync::Mutex::new(None)),
            initial_offset: 0,
            pause_tx: Arc::new(pause_tx),
            pause_rx,
        }
    }

    /// Seed the initial polling offset from persisted state.
    /// Must be called before `start()`.
    pub fn with_initial_offset(mut self, offset: i64) -> Self {
        self.initial_offset = offset;
        self
    }

    /// Pause the polling loop (level-triggered via watch channel).
    /// Safe to call from any thread / task.
    pub fn pause_polling(&self) {
        let _ = self.pause_tx.send(true);
    }

    /// Resume the polling loop.
    /// Safe to call from any thread / task.
    pub fn resume_polling(&self) {
        let _ = self.pause_tx.send(false);
    }

    fn parse_chat_id(chat_id: &str) -> Result<i64> {
        chat_id
            .parse::<i64>()
            .map_err(|_| anyhow!("Invalid chat_id: {}", chat_id))
    }

    /// Start polling, injecting a state update sender so the adapter can persist
    /// the Telegram offset after each successful batch.  This is a concrete-type
    /// extension method — it does not change the `ChatPlatform` trait signature.
    pub async fn start_with_state(
        &self,
        cmd_tx: mpsc::Sender<IncomingMessage>,
        state_tx: mpsc::Sender<StateUpdate>,
    ) -> Result<()> {
        let authorized_user_id = self.authorized_user_id;
        let connected = Arc::clone(&self.connected);
        let bot = self.bot.clone();
        let initial_offset = self.initial_offset;
        let mut pause_rx = self.pause_rx.clone();

        tokio::spawn(async move {
            // Delete any existing webhook so long-polling works
            tracing::info!("Telegram: deleting webhook (if any)...");
            match bot.delete_webhook().send().await {
                Ok(_) => tracing::info!("Telegram: webhook cleared"),
                Err(e) => tracing::warn!("Telegram: failed to delete webhook: {}", e),
            }

            connected.store(true, Ordering::SeqCst);
            tracing::info!(
                "Telegram: polling for updates (authorized_user_id={})",
                authorized_user_id
            );

            // Bounds-check the persisted offset on the way back in.  The
            // state store uses i64 for API parity, but teloxide's offset
            // parameter is i32.  Silently truncating with `as i32` could
            // wrap long-running offsets into negative territory (code-review
            // finding).  Clamp explicitly with a loud log.
            let raw_offset = initial_offset;
            let mut offset: i32 = if raw_offset < 0 {
                tracing::warn!(
                    "Telegram: persisted offset {} is negative, resetting to 0",
                    raw_offset
                );
                0
            } else if raw_offset > i32::MAX as i64 {
                tracing::error!(
                    "Telegram: persisted offset {} exceeds i32::MAX — backlog \
                     will be lost on this run. Resetting to 0.",
                    raw_offset
                );
                0
            } else {
                raw_offset as i32
            };
            let mut consecutive_failures: u32 = 0;

            loop {
                // Pause gate — block until unpaused (level-triggered watch channel).
                while *pause_rx.borrow() {
                    tracing::debug!("Telegram: polling paused — waiting for resume");
                    if pause_rx.changed().await.is_err() {
                        // Sender dropped — shutdown
                        connected.store(false, Ordering::SeqCst);
                        return;
                    }
                }

                let updates = bot
                    .get_updates()
                    .offset(offset)
                    .timeout(30)
                    .allowed_updates(vec![AllowedUpdate::Message])
                    .send()
                    .await;

                let updates: Vec<Update> = match updates {
                    Ok(updates) => {
                        if consecutive_failures > 0 {
                            tracing::info!(
                                "Telegram: polling recovered after {} consecutive failures",
                                consecutive_failures
                            );
                        }
                        consecutive_failures = 0;
                        updates
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        // Exponential backoff: 5s, 10s, 20s, 40s, capped at 60s
                        let backoff_secs = polling_backoff_secs(consecutive_failures);
                        if consecutive_failures == 1 {
                            tracing::error!("Telegram: getUpdates failed: {}", e);
                        } else {
                            tracing::warn!(
                                "Telegram: getUpdates failed ({} consecutive), retrying in {}s: {}",
                                consecutive_failures,
                                backoff_secs,
                                e
                            );
                        }
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        continue;
                    }
                };

                for update in updates {
                    offset = update.id.as_offset();

                    let message: Message = match update.kind {
                        UpdateKind::Message(msg) => msg,
                        _ => continue,
                    };

                    // Auth check
                    let from_id: u64 = match message.from.as_ref().map(|u| u.id.0) {
                        Some(id) => id,
                        None => continue,
                    };

                    if from_id != authorized_user_id {
                        tracing::debug!("Telegram: ignoring unauthorized user {}", from_id);
                        continue;
                    }

                    // Extract text from text messages or captions on photos/documents.
                    let text: String = message
                        .text()
                        .or_else(|| message.caption())
                        .unwrap_or("")
                        .to_string();

                    // Download any photo or image-document attachments.
                    let mut attachments: Vec<Attachment> = Vec::new();

                    // Photos: select the largest size (last element).
                    if let Some(photos) = message.photo() {
                        if let Some(photo) = photos.last() {
                            if photo.file.size > 20 * 1024 * 1024 {
                                tracing::warn!(
                                    "Telegram: photo too large ({} bytes), skipping",
                                    photo.file.size
                                );
                            } else {
                                match bot.get_file(&photo.file.id).send().await {
                                    Ok(tg_file) => {
                                        let path = PathBuf::from(format!(
                                            "/tmp/terminus-img-{}.jpg",
                                            Uuid::new_v4()
                                        ));
                                        match tokio::fs::File::create(&path).await {
                                            Ok(mut dst) => {
                                                match bot
                                                    .download_file(&tg_file.path, &mut dst)
                                                    .await
                                                {
                                                    Ok(_) => {
                                                        #[cfg(unix)]
                                                        {
                                                            use std::os::unix::fs::PermissionsExt;
                                                            let perms =
                                                                std::fs::Permissions::from_mode(
                                                                    0o600,
                                                                );
                                                            let _ = tokio::fs::set_permissions(
                                                                &path, perms,
                                                            )
                                                            .await;
                                                        }
                                                        attachments.push(Attachment {
                                                            filename: path
                                                                .file_name()
                                                                .unwrap()
                                                                .to_string_lossy()
                                                                .into_owned(),
                                                            path,
                                                            media_type: "image/jpeg".to_string(),
                                                        });
                                                    }
                                                    Err(e) => {
                                                        let _ = tokio::fs::remove_file(&path).await;
                                                        tracing::warn!(
                                                            "Telegram: photo download failed: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Telegram: failed to create temp file: {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Telegram: get_file failed for photo: {}",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Documents with an image mime type.
                    if let Some(doc) = message.document() {
                        let media_type_str = doc
                            .mime_type
                            .as_ref()
                            .map(|m| m.to_string())
                            .unwrap_or_default();
                        let is_image = media_type_str.starts_with("image/");
                        if is_image {
                            let media_type = media_type_str.clone();
                            // Derive a file extension from the subtype (e.g. "image/png" -> "png").
                            let ext = media_type_str.strip_prefix("image/").unwrap_or("bin");

                            if doc.file.size > 20 * 1024 * 1024 {
                                tracing::warn!(
                                    "Telegram: document too large ({} bytes), skipping",
                                    doc.file.size
                                );
                            } else {
                                match bot.get_file(&doc.file.id).send().await {
                                    Ok(tg_file) => {
                                        let path = PathBuf::from(format!(
                                            "/tmp/terminus-img-{}.{}",
                                            Uuid::new_v4(),
                                            ext
                                        ));
                                        match tokio::fs::File::create(&path).await {
                                            Ok(mut dst) => {
                                                match bot
                                                    .download_file(&tg_file.path, &mut dst)
                                                    .await
                                                {
                                                    Ok(_) => {
                                                        #[cfg(unix)]
                                                        {
                                                            use std::os::unix::fs::PermissionsExt;
                                                            let perms =
                                                                std::fs::Permissions::from_mode(
                                                                    0o600,
                                                                );
                                                            let _ = tokio::fs::set_permissions(
                                                                &path, perms,
                                                            )
                                                            .await;
                                                        }
                                                        attachments.push(Attachment {
                                                            filename: path
                                                                .file_name()
                                                                .unwrap()
                                                                .to_string_lossy()
                                                                .into_owned(),
                                                            path,
                                                            media_type,
                                                        });
                                                    }
                                                    Err(e) => {
                                                        let _ = tokio::fs::remove_file(&path).await;
                                                        tracing::warn!(
                                                            "Telegram: document download failed: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Telegram: failed to create temp file for document: {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Telegram: get_file failed for document: {}",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Skip messages with no text and no attachments.
                    if text.is_empty() && attachments.is_empty() {
                        continue;
                    }

                    let chat_id = message.chat.id.0.to_string();
                    tracing::debug!(
                        "Telegram: received message from chat {} ({} chars, {} attachments)",
                        chat_id,
                        text.len(),
                        attachments.len()
                    );

                    let incoming = IncomingMessage {
                        user_id: from_id.to_string(),
                        text,
                        platform: PlatformType::Telegram,
                        reply_context: ReplyContext {
                            platform: PlatformType::Telegram,
                            chat_id,
                            thread_ts: None,
                            socket_reply_tx: None,
                        },
                        attachments,
                        socket_request_id: None,
                        socket_client_name: None,
                    };

                    if cmd_tx.send(incoming).await.is_err() {
                        tracing::warn!("Telegram: command channel closed, stopping");
                        connected.store(false, Ordering::SeqCst);
                        return;
                    }
                }

                // Persist the offset after each successful batch that advanced it.
                // Non-blocking: if the receiver is gone (App shut down) we just skip.
                if offset > 0 {
                    let _ = state_tx.try_send(StateUpdate::TelegramOffset(offset as i64));
                }

                if cmd_tx.is_closed() {
                    tracing::info!("Telegram: command channel closed, stopping");
                    break;
                }
            }

            connected.store(false, Ordering::SeqCst);
            tracing::info!("Telegram: polling stopped");
        });

        Ok(())
    }
}

#[async_trait]
impl ChatPlatform for TelegramAdapter {
    /// Start long-polling without state persistence.  Callers that need
    /// offset persistence should use `start_with_state` instead.
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        // Use a throw-away channel; offset updates are silently discarded.
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(1);
        self.start_with_state(cmd_tx, state_tx).await
    }

    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let chat_id_i64 = Self::parse_chat_id(chat_id)?;
        let msg = self
            .bot
            .send_message(ChatId(chat_id_i64), text)
            .await
            .map_err(|e| anyhow!("Telegram send_message failed: {}", e))?;

        Ok(PlatformMessageId::Telegram(msg.id.0))
    }

    async fn edit_message(
        &self,
        msg_id: &PlatformMessageId,
        chat_id: &str,
        text: &str,
    ) -> Result<()> {
        let msg_id_i32 = match msg_id {
            PlatformMessageId::Telegram(id) => *id,
            _ => {
                return Err(anyhow!(
                    "edit_message called with non-Telegram PlatformMessageId"
                ))
            }
        };

        let chat_id_i64 = Self::parse_chat_id(chat_id)?;

        // Rate limiting — release lock before sleeping to avoid blocking concurrent calls
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

        self.bot
            .edit_message_text(
                ChatId(chat_id_i64),
                teloxide::types::MessageId(msg_id_i32),
                text,
            )
            .await
            .map_err(|e| anyhow!("Telegram edit_message_text failed: {}", e))?;

        Ok(())
    }

    async fn send_photo(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let chat_id_i64 = Self::parse_chat_id(chat_id)?;
        let input = InputFile::memory(data.to_vec()).file_name(filename.to_string());
        let mut req = self.bot.send_photo(ChatId(chat_id_i64), input);
        if let Some(cap) = caption {
            req = req.caption(cap);
        }
        let msg = req
            .await
            .map_err(|e| anyhow!("Telegram send_photo failed: {}", e))?;
        Ok(PlatformMessageId::Telegram(msg.id.0))
    }

    async fn send_document(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let chat_id_i64 = Self::parse_chat_id(chat_id)?;
        let input = InputFile::memory(data.to_vec()).file_name(filename.to_string());
        let mut req = self.bot.send_document(ChatId(chat_id_i64), input);
        if let Some(cap) = caption {
            req = req.caption(cap);
        }
        let msg = req
            .await
            .map_err(|e| anyhow!("Telegram send_document failed: {}", e))?;
        Ok(PlatformMessageId::Telegram(msg.id.0))
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn platform_type(&self) -> PlatformType {
        PlatformType::Telegram
    }

    async fn pause(&self) {
        self.pause_polling();
    }

    async fn resume(&self) {
        self.resume_polling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_first_failure_is_5s() {
        assert_eq!(polling_backoff_secs(1), 5);
    }

    #[test]
    fn backoff_doubles_each_failure() {
        assert_eq!(polling_backoff_secs(2), 10);
        assert_eq!(polling_backoff_secs(3), 20);
        assert_eq!(polling_backoff_secs(4), 40);
    }

    #[test]
    fn backoff_caps_at_60s() {
        assert_eq!(polling_backoff_secs(5), 60);
        assert_eq!(polling_backoff_secs(6), 60);
        assert_eq!(polling_backoff_secs(100), 60);
    }

    #[test]
    fn backoff_zero_failures_does_not_panic() {
        // Edge case: should not be called with 0, but must not panic
        let result = polling_backoff_secs(0);
        assert!((1..=60).contains(&result));
    }

    #[test]
    fn backoff_u32_max_does_not_overflow() {
        let result = polling_backoff_secs(u32::MAX);
        assert_eq!(result, 60);
    }

    #[test]
    fn with_initial_offset_stores_value() {
        let adapter =
            TelegramAdapter::new("token".to_string(), 12345, 2000).with_initial_offset(42);
        assert_eq!(adapter.initial_offset, 42);
    }

    #[test]
    fn with_initial_offset_default_is_zero() {
        let adapter = TelegramAdapter::new("token".to_string(), 12345, 2000);
        assert_eq!(adapter.initial_offset, 0);
    }

    #[test]
    fn pause_resume_no_panic() {
        let adapter = TelegramAdapter::new("token".to_string(), 12345, 2000);
        // Initially not paused
        assert!(!*adapter.pause_rx.borrow());
        adapter.pause_polling();
        assert!(*adapter.pause_rx.borrow());
        adapter.resume_polling();
        assert!(!*adapter.pause_rx.borrow());
        // Multiple resume calls are safe
        adapter.resume_polling();
        adapter.resume_polling();
        assert!(!*adapter.pause_rx.borrow());
        // Interleaved pause/resume
        adapter.pause_polling();
        adapter.pause_polling(); // idempotent
        assert!(*adapter.pause_rx.borrow());
        adapter.resume_polling();
        assert!(!*adapter.pause_rx.borrow());
    }
}
