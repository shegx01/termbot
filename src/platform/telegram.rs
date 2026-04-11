use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use teloxide::net::default_reqwest_settings;
use teloxide::payloads::GetUpdatesSetters;
use teloxide::prelude::*;
use teloxide::types::{AllowedUpdate, ChatId, UpdateKind};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use super::{ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext};

pub struct TelegramAdapter {
    bot: teloxide::Bot,
    authorized_user_id: u64,
    connected: Arc<AtomicBool>,
    rate_limit_ms: u64,
    last_edit: Arc<tokio::sync::Mutex<Option<Instant>>>,
}

impl TelegramAdapter {
    pub fn new(token: String, authorized_user_id: u64, rate_limit_ms: u64) -> Self {
        let client = default_reqwest_settings()
            .timeout(Duration::from_secs(45))
            .build()
            .expect("Failed to build reqwest client");
        let bot = teloxide::Bot::with_client(token, client);
        Self {
            bot,
            authorized_user_id,
            connected: Arc::new(AtomicBool::new(false)),
            rate_limit_ms,
            last_edit: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    fn parse_chat_id(chat_id: &str) -> Result<i64> {
        chat_id
            .parse::<i64>()
            .map_err(|_| anyhow!("Invalid chat_id: {}", chat_id))
    }
}

#[async_trait]
impl ChatPlatform for TelegramAdapter {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let authorized_user_id = self.authorized_user_id;
        let connected = Arc::clone(&self.connected);
        let bot = self.bot.clone();

        tokio::spawn(async move {
            // Delete any existing webhook so long-polling works
            tracing::info!("Telegram: deleting webhook (if any)...");
            match bot.delete_webhook().send().await {
                Ok(_) => tracing::info!("Telegram: webhook cleared"),
                Err(e) => tracing::warn!("Telegram: failed to delete webhook: {}", e),
            }

            connected.store(true, Ordering::SeqCst);
            tracing::info!("Telegram: polling for updates (authorized_user_id={})", authorized_user_id);

            let mut offset: i32 = 0;

            loop {
                let updates = bot
                    .get_updates()
                    .offset(offset)
                    .timeout(30)
                    .allowed_updates(vec![AllowedUpdate::Message])
                    .send()
                    .await;

                let updates: Vec<Update> = match updates {
                    Ok(updates) => updates,
                    Err(e) => {
                        tracing::error!("Telegram: getUpdates failed: {}", e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
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

                    let text: String = match message.text() {
                        Some(t) => t.to_string(),
                        None => continue,
                    };

                    let chat_id = message.chat.id.0.to_string();
                    tracing::debug!("Telegram: received message from chat {} ({} chars)", chat_id, text.len());

                    let incoming = IncomingMessage {
                        user_id: from_id.to_string(),
                        text,
                        platform: PlatformType::Telegram,
                        reply_context: ReplyContext {
                            platform: PlatformType::Telegram,
                            chat_id,
                            thread_ts: None,
                        },
                    };

                    if cmd_tx.send(incoming).await.is_err() {
                        tracing::warn!("Telegram: command channel closed, stopping");
                        connected.store(false, Ordering::SeqCst);
                        return;
                    }
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
            _ => return Err(anyhow!("edit_message called with non-Telegram PlatformMessageId")),
        };

        let chat_id_i64 = Self::parse_chat_id(chat_id)?;

        // Rate limiting — release lock before sleeping to avoid blocking concurrent calls
        let sleep_dur = {
            let mut last = self.last_edit.lock().await;
            let dur = if let Some(t) = *last {
                let elapsed = t.elapsed();
                let min_gap = Duration::from_millis(self.rate_limit_ms);
                if elapsed < min_gap { Some(min_gap - elapsed) } else { None }
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

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn platform_type(&self) -> PlatformType {
        PlatformType::Telegram
    }
}
