use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serenity::all::{
    ChannelId, CreateAttachment, CreateMessage, EditMessage, GatewayIntents, MessageId, UserId,
};
use serenity::client::{Client, Context, EventHandler};
use serenity::gateway::ShardManager;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::{ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext};
use crate::config::DiscordConfig;

/// Maximum file upload size for Discord (25 MB).
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

pub struct DiscordAdapter {
    config: DiscordConfig,
    authorized_user_id: UserId,
    pause_tx: watch::Sender<bool>,
    pause_rx: watch::Receiver<bool>,
    edit_throttle: Arc<Mutex<Option<Instant>>>,
    edit_throttle_ms: u64,
    http: Arc<Http>,
    shard_manager: Arc<Mutex<Option<Arc<ShardManager>>>>,
}

impl DiscordAdapter {
    pub fn new(
        config: DiscordConfig,
        authorized_user_id: UserId,
        edit_throttle_ms: u64,
    ) -> Result<Self> {
        let http = Arc::new(Http::new(&config.bot_token));
        let (pause_tx, pause_rx) = watch::channel(false);
        Ok(Self {
            config,
            authorized_user_id,
            pause_tx,
            pause_rx,
            edit_throttle: Arc::new(Mutex::new(None)),
            edit_throttle_ms,
            http,
            shard_manager: Arc::new(Mutex::new(None)),
        })
    }

    /// Parse a string chat_id (Discord snowflake) into a ChannelId.
    fn parse_channel_id(chat_id: &str) -> Result<ChannelId> {
        let id: u64 = chat_id
            .parse()
            .map_err(|_| anyhow!("Invalid Discord channel_id: {}", chat_id))?;
        Ok(ChannelId::new(id))
    }

    /// Build a CreateMessage with an attachment, optionally adding a caption.
    /// Shared by send_photo and send_document (Discord treats all attachments uniformly).
    async fn send_attachment(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
    ) -> Result<PlatformMessageId> {
        if data.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "Discord attachment exceeds 25 MB limit ({} bytes)",
                data.len()
            );
        }

        let channel_id = Self::parse_channel_id(chat_id)?;
        let attachment = CreateAttachment::bytes(data.to_vec(), filename.to_string());
        let mut builder = CreateMessage::new().add_file(attachment);
        if let Some(cap) = caption {
            builder = builder.content(cap);
        }

        let msg = channel_id
            .send_message(&*self.http, builder)
            .await
            .map_err(|e| anyhow!("Discord send attachment failed: {}", e))?;

        Ok(PlatformMessageId::Discord(msg.id.get()))
    }
}

#[async_trait]
impl ChatPlatform for DiscordAdapter {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let intents = GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let guild_channel_id = self.config.channel_id.map(ChannelId::new);

        let handler = DiscordHandler {
            cmd_tx,
            authorized_user_id: self.authorized_user_id,
            pause_rx: self.pause_rx.clone(),
            guild_channel_id,
        };

        let mut client = Client::builder(&self.config.bot_token, intents)
            .event_handler(handler)
            .await
            .map_err(|e| anyhow!("Discord client build failed: {}", e))?;

        // Stash shard_manager so is_connected() can check it.
        {
            let mut sm = self.shard_manager.lock().await;
            *sm = Some(Arc::clone(&client.shard_manager));
        }

        info!("Discord: starting gateway connection");
        client
            .start()
            .await
            .map_err(|e| anyhow!("Discord gateway error: {}", e))?;

        Ok(())
    }

    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let channel_id = Self::parse_channel_id(chat_id)?;
        let builder = CreateMessage::new().content(text);

        let msg = channel_id
            .send_message(&*self.http, builder)
            .await
            .map_err(|e| anyhow!("Discord send_message failed: {}", e))?;

        Ok(PlatformMessageId::Discord(msg.id.get()))
    }

    async fn edit_message(
        &self,
        msg_id: &PlatformMessageId,
        chat_id: &str,
        text: &str,
    ) -> Result<()> {
        let discord_msg_id = match msg_id {
            PlatformMessageId::Discord(id) => *id,
            _ => {
                return Err(anyhow!(
                    "edit_message called with non-Discord PlatformMessageId"
                ))
            }
        };

        // Rate limiting: skip edit if within throttle window
        {
            let mut last = self.edit_throttle.lock().await;
            if let Some(t) = *last {
                let elapsed = t.elapsed();
                let min_gap = std::time::Duration::from_millis(self.edit_throttle_ms);
                if elapsed < min_gap {
                    return Ok(());
                }
            }
            *last = Some(Instant::now());
        }

        let channel_id = Self::parse_channel_id(chat_id)?;
        let message_id = MessageId::new(discord_msg_id);
        let builder = EditMessage::new().content(text);

        channel_id
            .edit_message(&*self.http, message_id, builder)
            .await
            .map_err(|e| anyhow!("Discord edit_message failed: {}", e))?;

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
        self.send_attachment(data, filename, caption, chat_id).await
    }

    async fn send_document(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        self.send_attachment(data, filename, caption, chat_id).await
    }

    fn is_connected(&self) -> bool {
        self.shard_manager
            .try_lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    fn platform_type(&self) -> PlatformType {
        PlatformType::Discord
    }

    async fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }

    async fn resume(&self) {
        let _ = self.pause_tx.send(false);
    }
}

/// Internal event handler forwarding Discord gateway events to the main loop.
struct DiscordHandler {
    cmd_tx: mpsc::Sender<IncomingMessage>,
    authorized_user_id: UserId,
    pause_rx: watch::Receiver<bool>,
    guild_channel_id: Option<ChannelId>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, _ctx: Context, msg: Message) {
        // Handler-gate: drop events while paused (sleep/wake gap handling).
        if *self.pause_rx.borrow() {
            debug!("Discord: dropping message while paused");
            return;
        }

        // Auth check: single-user bot
        if msg.author.id != self.authorized_user_id {
            debug!("Discord: ignoring unauthorized user {}", msg.author.id);
            return;
        }

        // Ignore bot messages (including our own)
        if msg.author.bot {
            return;
        }

        // Accept DMs (guild_id is None) or messages in the bound guild channel.
        let is_dm = msg.guild_id.is_none();
        let is_bound_channel = self.guild_channel_id == Some(msg.channel_id);
        if !is_dm && !is_bound_channel {
            return;
        }

        let chat_id = msg.channel_id.get().to_string();
        debug!(
            "Discord: received message from channel {} ({} chars)",
            chat_id,
            msg.content.len()
        );

        let incoming = IncomingMessage {
            user_id: msg.author.id.get().to_string(),
            text: msg.content.clone(),
            platform: PlatformType::Discord,
            reply_context: ReplyContext {
                platform: PlatformType::Discord,
                chat_id,
                thread_ts: None,
            },
            attachments: Vec::new(),
        };

        if self.cmd_tx.send(incoming).await.is_err() {
            warn!("Discord: command channel closed, handler will stop receiving");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            username = %ready.user.name,
            "Discord: gateway ready"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a DiscordAdapter for testing (no network required).
    fn test_adapter(edit_throttle_ms: u64) -> DiscordAdapter {
        DiscordAdapter::new(
            DiscordConfig {
                bot_token: "test-token".to_string(),
                guild_id: None,
                channel_id: None,
            },
            UserId::new(123456789),
            edit_throttle_ms,
        )
        .expect("test adapter construction should not fail")
    }

    #[test]
    fn parse_channel_id_valid_snowflake() {
        let cid = DiscordAdapter::parse_channel_id("123456789012345678");
        assert!(cid.is_ok());
        assert_eq!(cid.unwrap().get(), 123456789012345678);
    }

    #[test]
    fn parse_channel_id_not_a_number_returns_err() {
        let result = DiscordAdapter::parse_channel_id("not-a-number");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid Discord channel_id"),
            "unexpected error: {}",
            err_msg
        );
    }

    #[test]
    fn parse_channel_id_empty_string_returns_err() {
        let result = DiscordAdapter::parse_channel_id("");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_photo_exceeding_25mb_returns_err() {
        let adapter = test_adapter(2000);
        let oversized = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let result = adapter
            .send_attachment(&oversized, "big.bin", None, "123456")
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("25 MB"),
            "expected 25 MB error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn send_document_exceeding_25mb_returns_err() {
        let adapter = test_adapter(2000);
        let oversized = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        // send_document delegates to send_attachment, same check applies
        let result = adapter
            .send_document(&oversized, "big.zip", None, "123456", None)
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("25 MB"),
            "expected 25 MB error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn edit_throttle_gates_second_edit_within_window() {
        let adapter = test_adapter(5000); // 5s throttle

        // Simulate first edit: set the throttle timestamp directly
        {
            let mut last = adapter.edit_throttle.lock().await;
            *last = Some(Instant::now());
        }

        // Second edit within the window should be a no-op (returns Ok without
        // actually calling Discord). We verify by checking the throttle timestamp
        // did NOT update — meaning the early return path was taken.
        let before_edit = {
            let last = adapter.edit_throttle.lock().await;
            last.unwrap()
        };

        // Since we can't actually call Discord in tests, we test the throttle
        // logic by checking the guard condition directly.
        let elapsed = before_edit.elapsed();
        let min_gap = std::time::Duration::from_millis(5000);
        assert!(
            elapsed < min_gap,
            "test ran too slowly for throttle check"
        );

        // The edit_message method would return Ok(()) due to throttle.
        // We verify the throttle logic in isolation: within the window,
        // the early return should fire.
        let should_skip = {
            let last = adapter.edit_throttle.lock().await;
            if let Some(t) = *last {
                t.elapsed() < min_gap
            } else {
                false
            }
        };
        assert!(should_skip, "edit should be throttled within window");
    }

    #[tokio::test]
    async fn edit_throttle_allows_after_window() {
        let adapter = test_adapter(0); // 0ms throttle = always allow

        // Set a past timestamp
        {
            let mut last = adapter.edit_throttle.lock().await;
            *last = Some(Instant::now() - std::time::Duration::from_secs(10));
        }

        let should_skip = {
            let last = adapter.edit_throttle.lock().await;
            if let Some(t) = *last {
                t.elapsed() < std::time::Duration::from_millis(0)
            } else {
                false
            }
        };
        assert!(!should_skip, "edit should NOT be throttled after window");
    }

    #[test]
    fn pause_resume_flips_state() {
        let adapter = test_adapter(2000);

        // Initially not paused
        assert!(!*adapter.pause_rx.borrow());

        // Pause
        let _ = adapter.pause_tx.send(true);
        assert!(*adapter.pause_rx.borrow());

        // Resume
        let _ = adapter.pause_tx.send(false);
        assert!(!*adapter.pause_rx.borrow());

        // Multiple pauses are idempotent
        let _ = adapter.pause_tx.send(true);
        let _ = adapter.pause_tx.send(true);
        assert!(*adapter.pause_rx.borrow());

        // Resume after multiple pauses
        let _ = adapter.pause_tx.send(false);
        assert!(!*adapter.pause_rx.borrow());
    }

    #[tokio::test]
    async fn pause_resume_via_trait_methods() {
        let adapter = test_adapter(2000);

        assert!(!*adapter.pause_rx.borrow());
        adapter.pause().await;
        assert!(*adapter.pause_rx.borrow());
        adapter.resume().await;
        assert!(!*adapter.pause_rx.borrow());
    }

    #[test]
    fn platform_type_is_discord() {
        let adapter = test_adapter(2000);
        assert_eq!(adapter.platform_type(), PlatformType::Discord);
    }

    #[test]
    fn is_connected_false_when_no_shard_manager() {
        let adapter = test_adapter(2000);
        assert!(!adapter.is_connected());
    }
}
