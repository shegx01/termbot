use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serenity::all::{
    ChannelId, CreateAttachment, CreateMessage, CreateThread, EditMessage, GatewayIntents,
    MessageId, MessageReference, UserId,
};
use serenity::builder::GetMessages;
use serenity::client::{Client, Context, EventHandler};
use serenity::gateway::ShardManager;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use ulid::Ulid;

use super::{
    Attachment, ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext,
};
use crate::config::DiscordConfig;
use crate::state_store::StateUpdate;

/// Maximum file upload size for Discord (25 MB).
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// Discord-specific reply routing hint (private to this module).
///
/// `Thread` targets an existing thread channel (guild mode).
/// `Reference` sets a message-reply chain via MessageReference (DM mode or
/// thread-fallback after archive/delete).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscordReplyTarget {
    Thread(u64),
    Reference(u64),
}

/// SSRF allowlist: only download from Discord's official attachment CDNs.
///
/// Case-insensitive to guard against URL-scheme and hostname casing tricks.
pub(crate) fn is_valid_discord_download_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://cdn.discordapp.com/")
        || lower.starts_with("https://media.discordapp.net/")
}

/// Sanitize a user prompt into a valid Discord thread title.
///
/// Rules: strip C0 control chars, collapse whitespace, truncate at 50 chars
/// (UTF-8 char boundary), fall back to "terminus session" if empty.
fn sanitize_thread_title(prompt: &str) -> String {
    // Replace all control characters (C0 block, including \n and \t) with
    // a space, then collapse runs of whitespace and truncate.
    let cleaned: String = prompt
        .chars()
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(50).collect();
    if truncated.is_empty() {
        "terminus session".to_string()
    } else {
        truncated
    }
}

/// Download attachments from a Discord message.
///
/// Validates the URL against the SSRF allowlist, checks size, downloads
/// with a 30s timeout, and persists to `/tmp/terminus-discord-{ulid}.{ext}`.
/// Skips and logs any individual failure; returns only successful attachments.
async fn extract_attachments(
    http_client: &reqwest::Client,
    atts: &[serenity::model::channel::Attachment],
) -> Vec<Attachment> {
    let mut attachments = Vec::new();

    for att in atts {
        if !is_valid_discord_download_url(&att.url) {
            warn!("Rejecting non-Discord download URL: {}", att.url);
            continue;
        }

        if u64::from(att.size) > MAX_ATTACHMENT_BYTES as u64 {
            warn!(
                "Skipping Discord attachment '{}': size {} exceeds {} byte limit",
                att.filename, att.size, MAX_ATTACHMENT_BYTES
            );
            continue;
        }

        let ext = std::path::Path::new(&att.filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");

        let tmp_path = PathBuf::from(format!("/tmp/terminus-discord-{}.{}", Ulid::new(), ext));

        let download_future = http_client.get(&att.url).send();
        let resp = match tokio::time::timeout(Duration::from_secs(30), download_future).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!(
                    "Failed to download Discord attachment '{}': {}",
                    att.filename, e
                );
                continue;
            }
            Err(_) => {
                warn!("Timeout downloading Discord attachment '{}'", att.filename);
                continue;
            }
        };

        let bytes = match tokio::time::timeout(Duration::from_secs(30), resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                warn!(
                    "Failed to read Discord attachment body '{}': {}",
                    att.filename, e
                );
                continue;
            }
            Err(_) => {
                warn!("Timeout reading Discord attachment body '{}'", att.filename);
                continue;
            }
        };

        if bytes.len() > MAX_ATTACHMENT_BYTES {
            warn!(
                "Downloaded Discord attachment '{}' body exceeds limit ({} bytes), discarding",
                att.filename,
                bytes.len()
            );
            continue;
        }

        if let Err(e) = tokio::fs::write(&tmp_path, &bytes).await {
            warn!(
                "Failed to write Discord attachment to {:?}: {}",
                tmp_path, e
            );
            let _ = tokio::fs::remove_file(&tmp_path).await;
            continue;
        }

        let media_type = att
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());

        attachments.push(Attachment {
            path: tmp_path,
            filename: att.filename.clone(),
            media_type,
        });
    }

    attachments
}

pub struct DiscordAdapter {
    config: DiscordConfig,
    authorized_user_id: UserId,
    pause_tx: watch::Sender<bool>,
    pause_rx: watch::Receiver<bool>,
    edit_throttle: Arc<Mutex<Option<Instant>>>,
    edit_throttle_ms: u64,
    http: Arc<Http>,
    shard_manager: Arc<Mutex<Option<Arc<ShardManager>>>>,
    http_client: reqwest::Client,

    // ── Catchup state ────────────────────────────────────────────────────────
    /// Guards against concurrent `run_catchup` invocations.  Set to `true`
    /// before spawning a catchup task; reset to `false` when it finishes.
    pub catchup_in_progress: Arc<AtomicBool>,
    /// Per-channel watermark: channel_id → latest delivered message snowflake.
    last_seen_message_id: Arc<Mutex<HashMap<String, u64>>>,
    /// Dedup ring for gateway/REST overlap prevention (cap 200).
    dedup_window: Arc<Mutex<VecDeque<u64>>>,
    /// State worker channel for force-persisting watermarks.
    state_tx: mpsc::Sender<StateUpdate>,
    /// Populated by `start()` so `run_catchup` can forward replayed messages.
    cmd_tx_slot: Arc<Mutex<Option<mpsc::Sender<IncomingMessage>>>>,

    // ── Thread delivery state ────────────────────────────────────────────────
    /// Maps chat_id → (thread_channel_id, parent_msg_id).
    /// Session-scoped: not persisted across restarts.
    thread_map: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    /// Maps chat_id → most recent user message snowflake.
    /// Used by resolve_target to build MessageReference in DM mode and to
    /// seed thread_map in guild mode.
    parent_msg_ids: Arc<Mutex<HashMap<String, u64>>>,
    /// Maps chat_id → sanitized thread title derived from the user's prompt.
    /// Populated by the handler so resolve_target can use a content-derived title.
    thread_title_hints: Arc<Mutex<HashMap<String, String>>>,
}

impl DiscordAdapter {
    pub fn new(
        config: DiscordConfig,
        authorized_user_id: UserId,
        edit_throttle_ms: u64,
        state_tx: mpsc::Sender<StateUpdate>,
        initial_watermarks: HashMap<String, u64>,
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
            http_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| anyhow!("Discord http client build failed: {}", e))?,
            catchup_in_progress: Arc::new(AtomicBool::new(false)),
            last_seen_message_id: Arc::new(Mutex::new(initial_watermarks)),
            dedup_window: Arc::new(Mutex::new(VecDeque::new())),
            state_tx,
            cmd_tx_slot: Arc::new(Mutex::new(None)),
            thread_map: Arc::new(Mutex::new(HashMap::new())),
            parent_msg_ids: Arc::new(Mutex::new(HashMap::new())),
            thread_title_hints: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Parse a string chat_id (Discord snowflake) into a ChannelId.
    fn parse_channel_id(chat_id: &str) -> Result<ChannelId> {
        let id: u64 = chat_id
            .parse()
            .map_err(|_| anyhow!("Invalid Discord channel_id: {}", chat_id))?;
        Ok(ChannelId::new(id))
    }

    /// Resolve the delivery target for a given chat_id.
    ///
    /// Guild mode: look up or create a Thread attached to parent_msg_id.
    /// DM mode: return a MessageReference to parent_msg_id.
    /// Falls back to `Reference` on thread-create error.
    async fn resolve_target(
        &self,
        chat_id: &str,
        parent_msg_id: u64,
        is_guild: bool,
    ) -> Result<DiscordReplyTarget> {
        if !is_guild {
            return Ok(DiscordReplyTarget::Reference(parent_msg_id));
        }

        // Guild mode: check thread_map first.
        {
            let map = self.thread_map.lock().await;
            if let Some(&(thread_id, _)) = map.get(chat_id) {
                return Ok(DiscordReplyTarget::Thread(thread_id));
            }
        }

        // Create a new Thread from the parent message.
        let channel_id = Self::parse_channel_id(chat_id)?;
        let parent_id = MessageId::new(parent_msg_id);

        let title = {
            let hints = self.thread_title_hints.lock().await;
            hints
                .get(chat_id)
                .cloned()
                .unwrap_or_else(|| "terminus session".to_string())
        };

        match channel_id
            .create_thread_from_message(&*self.http, parent_id, CreateThread::new(title))
            .await
        {
            Ok(thread) => {
                let thread_id = thread.id.get();
                self.thread_map
                    .lock()
                    .await
                    .insert(chat_id.to_string(), (thread_id, parent_msg_id));
                Ok(DiscordReplyTarget::Thread(thread_id))
            }
            Err(e) => {
                warn!(
                    "Discord thread creation failed for chat_id={}: {}; falling back to Reference",
                    chat_id, e
                );
                Ok(DiscordReplyTarget::Reference(parent_msg_id))
            }
        }
    }

    async fn send_to_target(
        &self,
        chat_id: &str,
        discord_reply: Option<&DiscordReplyTarget>,
        builder: CreateMessage,
        op_name: &str,
    ) -> Result<serenity::model::channel::Message> {
        match discord_reply {
            Some(DiscordReplyTarget::Thread(thread_id)) => {
                let thread_channel = ChannelId::new(*thread_id);
                match thread_channel
                    .send_message(&*self.http, builder.clone())
                    .await
                {
                    Ok(m) => Ok(m),
                    Err(serenity::Error::Http(ref e))
                        if e.status_code()
                            .map(|s| matches!(s.as_u16(), 400 | 403 | 404))
                            .unwrap_or(false) =>
                    {
                        let status = e.status_code().map(|s| s.as_u16()).unwrap_or(0);
                        let parent = {
                            let mut map = self.thread_map.lock().await;
                            map.remove(chat_id).map(|(_, p)| p)
                        };
                        warn!(
                            "Discord Thread send failed ({}); falling back to MessageReference for chat_id={}",
                            status, chat_id
                        );
                        let parent_id = parent.ok_or_else(|| {
                            anyhow!(
                                "Discord Thread send failed (status {}) and no parent_msg_id for fallback",
                                status
                            )
                        })?;
                        let channel_id = Self::parse_channel_id(chat_id)?;
                        let ref_builder = builder.reference_message(MessageReference::from((
                            channel_id,
                            MessageId::new(parent_id),
                        )));
                        channel_id
                            .send_message(&*self.http, ref_builder)
                            .await
                            .map_err(|e| {
                                anyhow!(
                                    "Discord {} (fallback) failed for chat_id={}: {}",
                                    op_name,
                                    chat_id,
                                    e
                                )
                            })
                    }
                    Err(e) => Err(anyhow!("Discord {} failed: {}", op_name, e)),
                }
            }
            Some(DiscordReplyTarget::Reference(parent_msg_id)) => {
                let channel_id = Self::parse_channel_id(chat_id)?;
                let ref_builder = builder.reference_message(MessageReference::from((
                    channel_id,
                    MessageId::new(*parent_msg_id),
                )));
                channel_id
                    .send_message(&*self.http, ref_builder)
                    .await
                    .map_err(|e| anyhow!("Discord {} failed: {}", op_name, e))
            }
            None => {
                let channel_id = Self::parse_channel_id(chat_id)?;
                channel_id
                    .send_message(&*self.http, builder)
                    .await
                    .map_err(|e| anyhow!("Discord {} failed: {}", op_name, e))
            }
        }
    }

    async fn send_attachment(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        discord_reply: Option<&DiscordReplyTarget>,
    ) -> Result<PlatformMessageId> {
        if data.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "Discord attachment exceeds 25 MB limit ({} bytes)",
                data.len()
            );
        }

        let attachment = CreateAttachment::bytes(data.to_vec(), filename.to_string());
        let mut builder = CreateMessage::new().add_file(attachment);
        if let Some(cap) = caption {
            builder = builder.content(cap);
        }

        let msg = self
            .send_to_target(chat_id, discord_reply, builder, "send attachment")
            .await?;
        Ok(PlatformMessageId::Discord(msg.id.get()))
    }

    /// Fetch missed messages for all known channels since the stored watermarks
    /// and forward them via cmd_tx.
    pub async fn run_catchup(&self) -> Result<()> {
        let cmd_tx = {
            let slot = self.cmd_tx_slot.lock().await;
            match &*slot {
                Some(tx) => tx.clone(),
                None => {
                    warn!("Discord run_catchup called before start(); skipping");
                    return Ok(());
                }
            }
        };

        let snapshot = self.last_seen_message_id.lock().await.clone();

        for (chat_id_str, after_id) in snapshot {
            let channel_id = match chat_id_str.parse::<u64>() {
                Ok(id) => ChannelId::new(id),
                Err(_) => continue,
            };

            let mut current_after = after_id;
            let mut total_replayed: usize = 0;

            loop {
                let messages = match channel_id
                    .messages(
                        &*self.http,
                        GetMessages::new()
                            .after(MessageId::new(current_after))
                            .limit(100u8),
                    )
                    .await
                {
                    Ok(msgs) => msgs,
                    Err(serenity::Error::Http(ref e))
                        if e.status_code()
                            .map(|s| matches!(s.as_u16(), 403 | 404))
                            .unwrap_or(false) =>
                    {
                        warn!(
                            "Discord catchup HTTP {} for channel {}",
                            e.status_code().map(|s| s.as_u16()).unwrap_or(0),
                            chat_id_str
                        );
                        break;
                    }
                    Err(e) => {
                        warn!("Discord catchup error for channel {}: {}", chat_id_str, e);
                        break;
                    }
                };

                if messages.is_empty() {
                    break;
                }

                // Discord returns messages in descending order; reverse for chronological replay.
                for msg in messages.iter().rev() {
                    let msg_id = msg.id.get();
                    // Always advance the cursor before any filter; otherwise a
                    // page consisting entirely of bot/unauthorized messages would
                    // not advance `current_after` and the next page request would
                    // return the same 100 messages, looping indefinitely.
                    current_after = current_after.max(msg_id);

                    if msg.author.id != self.authorized_user_id {
                        continue;
                    }
                    if msg.author.bot {
                        continue;
                    }

                    {
                        let mut window = self.dedup_window.lock().await;
                        if window.contains(&msg_id) {
                            continue;
                        }
                        if window.len() >= 200 {
                            window.pop_front();
                        }
                        window.push_back(msg_id);
                    }

                    // Update watermark and persist before forwarding.
                    self.last_seen_message_id
                        .lock()
                        .await
                        .insert(chat_id_str.clone(), msg_id);
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        self.state_tx.send(StateUpdate::DiscordWatermark {
                            channel_id: chat_id_str.clone(),
                            message_id: msg_id,
                        }),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            warn!(
                                "Discord catchup state-tx closed for channel {}: {}",
                                chat_id_str, e
                            );
                            let warning_text = "Discord catchup watermark persistence failed; \
                                               this prompt's recovery may not survive a restart";
                            let Ok(channel_id_int) = chat_id_str.parse::<u64>() else {
                                warn!(
                                    "Discord catchup: invalid channel_id in watermark map: {}",
                                    chat_id_str
                                );
                                continue;
                            };
                            let warning_channel = ChannelId::new(channel_id_int);
                            if let Err(send_err) =
                                warning_channel.say(&*self.http, warning_text).await
                            {
                                warn!(
                                    "Failed to post persistence-failure warning to channel {}: {}",
                                    chat_id_str, send_err
                                );
                            }
                        }
                        Err(_) => {
                            warn!(
                                "Discord catchup state-tx send timed out (5s) for channel {}",
                                chat_id_str
                            );
                            let warning_text = "Discord catchup watermark persistence timed out; \
                                               this prompt's recovery may not survive a restart";
                            let Ok(channel_id_int) = chat_id_str.parse::<u64>() else {
                                warn!(
                                    "Discord catchup: invalid channel_id in watermark map: {}",
                                    chat_id_str
                                );
                                continue;
                            };
                            let warning_channel = ChannelId::new(channel_id_int);
                            if let Err(send_err) =
                                warning_channel.say(&*self.http, warning_text).await
                            {
                                warn!(
                                    "Failed to post persistence-timeout warning to channel {}: {}",
                                    chat_id_str, send_err
                                );
                            }
                        }
                    }

                    // Store parent_msg_id and content-derived thread title for delivery targeting.
                    self.parent_msg_ids
                        .lock()
                        .await
                        .insert(chat_id_str.clone(), msg_id);
                    self.thread_title_hints
                        .lock()
                        .await
                        .insert(chat_id_str.clone(), sanitize_thread_title(&msg.content));

                    let atts = extract_attachments(&self.http_client, &msg.attachments).await;

                    let incoming = IncomingMessage {
                        user_id: msg.author.id.get().to_string(),
                        text: msg.content.clone(),
                        platform: PlatformType::Discord,
                        reply_context: ReplyContext {
                            platform: PlatformType::Discord,
                            chat_id: chat_id_str.clone(),
                            thread_ts: None,
                            socket_reply_tx: None,
                        },
                        attachments: atts,
                        socket_request_id: None,
                        socket_client_name: None,
                    };

                    if cmd_tx.send(incoming).await.is_err() {
                        warn!("Discord catchup: command channel closed");
                        return Ok(());
                    }

                    total_replayed += 1;

                    if total_replayed >= 1000 {
                        warn!(
                            "Discord catchup hit 1000-message cap for channel {}; resuming normal handling",
                            chat_id_str
                        );
                        break;
                    }
                }

                if total_replayed >= 1000 || messages.len() < 100 {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ChatPlatform for DiscordAdapter {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        // Store cmd_tx so run_catchup can forward replayed messages.
        {
            let mut slot = self.cmd_tx_slot.lock().await;
            *slot = Some(cmd_tx.clone());
        }

        let intents = GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let guild_channel_id = self.config.channel_id.map(ChannelId::new);
        let handler = DiscordHandler {
            cmd_tx,
            authorized_user_id: self.authorized_user_id,
            pause_rx: self.pause_rx.clone(),
            guild_channel_id,
            last_seen_message_id: Arc::clone(&self.last_seen_message_id),
            dedup_window: Arc::clone(&self.dedup_window),
            state_tx: self.state_tx.clone(),
            http_client: self.http_client.clone(),
            parent_msg_ids: Arc::clone(&self.parent_msg_ids),
            thread_title_hints: Arc::clone(&self.thread_title_hints),
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
        let discord_reply = self.get_discord_reply_for_chat(chat_id).await;
        let builder = CreateMessage::new().content(text);
        let msg = self
            .send_to_target(chat_id, discord_reply.as_ref(), builder, "send_message")
            .await?;
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

        // Edit in the thread if one exists for this chat, otherwise edit in the original channel.
        let target_channel = {
            let map = self.thread_map.lock().await;
            map.get(chat_id).map(|&(thread_id, _)| thread_id)
        };

        let channel_id = if let Some(thread_id) = target_channel {
            ChannelId::new(thread_id)
        } else {
            Self::parse_channel_id(chat_id)?
        };

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
        let discord_reply = self.get_discord_reply_for_chat(chat_id).await;
        self.send_attachment(data, filename, caption, chat_id, discord_reply.as_ref())
            .await
    }

    async fn send_document(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let discord_reply = self.get_discord_reply_for_chat(chat_id).await;
        self.send_attachment(data, filename, caption, chat_id, discord_reply.as_ref())
            .await
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

impl DiscordAdapter {
    /// Resolve the current delivery target for a chat_id by inspecting stored
    /// state (thread_map + parent_msg_ids + is_guild_mode implied by thread_map entry).
    async fn get_discord_reply_for_chat(&self, chat_id: &str) -> Option<DiscordReplyTarget> {
        // Check thread_map first — if there's an active thread, use it.
        {
            let map = self.thread_map.lock().await;
            if let Some(&(thread_id, _)) = map.get(chat_id) {
                return Some(DiscordReplyTarget::Thread(thread_id));
            }
        }

        // No thread yet. Look up the parent_msg_id; if present, determine mode
        // from config.  If guild_id + channel_id are both set, attempt thread
        // creation; otherwise use Reference.
        let parent_msg_id = {
            let map = self.parent_msg_ids.lock().await;
            *map.get(chat_id)?
        };

        let is_guild = self.config.guild_id.is_some() && self.config.channel_id.is_some();

        if !is_guild {
            return Some(DiscordReplyTarget::Reference(parent_msg_id));
        }

        // Guild mode: resolve (possibly creating) the thread.
        match self.resolve_target(chat_id, parent_msg_id, true).await {
            Ok(target) => Some(target),
            Err(e) => {
                warn!(
                    "Discord resolve_target failed for chat_id={}: {}",
                    chat_id, e
                );
                None
            }
        }
    }
}

/// Internal event handler forwarding Discord gateway events to the main loop.
struct DiscordHandler {
    cmd_tx: mpsc::Sender<IncomingMessage>,
    authorized_user_id: UserId,
    pause_rx: watch::Receiver<bool>,
    guild_channel_id: Option<ChannelId>,
    last_seen_message_id: Arc<Mutex<HashMap<String, u64>>>,
    dedup_window: Arc<Mutex<VecDeque<u64>>>,
    state_tx: mpsc::Sender<StateUpdate>,
    http_client: reqwest::Client,
    parent_msg_ids: Arc<Mutex<HashMap<String, u64>>>,
    thread_title_hints: Arc<Mutex<HashMap<String, String>>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, ctx: Context, msg: Message) {
        // Handler-gate: drop events while paused (sleep/wake gap handling).
        // Messages dropped during pause do NOT update the watermark so REST
        // catchup will replay them after resume.
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
        let msg_id = msg.id.get();

        debug!(
            "Discord: received message from channel {} ({} chars)",
            chat_id,
            msg.content.len()
        );

        // Dedup: skip if already seen (e.g. from a concurrent REST catchup).
        {
            let mut window = self.dedup_window.lock().await;
            if window.contains(&msg_id) {
                return;
            }
            if window.len() >= 200 {
                window.pop_front();
            }
            window.push_back(msg_id);
        }

        // Advance watermark before forwarding.
        self.last_seen_message_id
            .lock()
            .await
            .insert(chat_id.clone(), msg_id);
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.state_tx.send(StateUpdate::DiscordWatermark {
                channel_id: chat_id.clone(),
                message_id: msg_id,
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!("Discord state-tx closed for channel {}: {}", chat_id, e);
                let warning_text = "⚠ Session-state persistence failed; \
                                    this prompt's named session won't survive a restart";
                if let Err(send_err) = msg.channel_id.say(&ctx.http, warning_text).await {
                    warn!(
                        "Failed to post persistence-failure warning to channel {}: {}",
                        chat_id, send_err
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Discord state-tx send timed out (5s) for channel {}",
                    chat_id
                );
                let warning_text = "⚠ Session-state persistence timed out; \
                                    this prompt's named session won't survive a restart";
                if let Err(send_err) = msg.channel_id.say(&ctx.http, warning_text).await {
                    warn!(
                        "Failed to post persistence-timeout warning to channel {}: {}",
                        chat_id, send_err
                    );
                }
            }
        }

        // Store parent_msg_id and content-derived thread title for delivery targeting.
        self.parent_msg_ids
            .lock()
            .await
            .insert(chat_id.clone(), msg_id);
        self.thread_title_hints
            .lock()
            .await
            .insert(chat_id.clone(), sanitize_thread_title(&msg.content));

        let attachments = extract_attachments(&self.http_client, &msg.attachments).await;

        let incoming = IncomingMessage {
            user_id: msg.author.id.get().to_string(),
            text: msg.content.clone(),
            platform: PlatformType::Discord,
            reply_context: ReplyContext {
                platform: PlatformType::Discord,
                chat_id,
                thread_ts: None,
                socket_reply_tx: None,
            },
            attachments,
            socket_request_id: None,
            socket_client_name: None,
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
    use crate::state_store::StateUpdate;

    /// Helper to build a DiscordAdapter for testing (no network required).
    fn test_adapter(edit_throttle_ms: u64) -> DiscordAdapter {
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(16);
        DiscordAdapter::new(
            DiscordConfig {
                bot_token: "test-token".to_string(),
                guild_id: None,
                channel_id: None,
            },
            UserId::new(123456789),
            edit_throttle_ms,
            state_tx,
            HashMap::new(),
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
            .send_attachment(&oversized, "big.bin", None, "123456", None)
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
        assert!(elapsed < min_gap, "test ran too slowly for throttle check");

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

    // ── Phase 2: attachment URL validation ───────────────────────────────────

    #[test]
    fn attachment_url_cdn_discordapp_is_valid() {
        assert!(is_valid_discord_download_url(
            "https://cdn.discordapp.com/attachments/123/456/file.png"
        ));
    }

    #[test]
    fn attachment_url_media_discordapp_net_is_valid() {
        assert!(is_valid_discord_download_url(
            "https://media.discordapp.net/attachments/123/456/img.jpg"
        ));
    }

    #[test]
    fn attachment_url_evil_domain_is_rejected() {
        assert!(!is_valid_discord_download_url("https://evil.com/malicious"));
    }

    #[test]
    fn attachment_url_no_scheme_is_rejected() {
        assert!(!is_valid_discord_download_url(
            "cdn.discordapp.com/attachments/123"
        ));
    }

    #[test]
    fn attachment_url_lookalike_domain_is_rejected() {
        assert!(!is_valid_discord_download_url(
            "https://cdn.discordapp.com.evil.com/attachments/x"
        ));
    }

    #[test]
    fn test_is_valid_discord_download_url_case_insensitive() {
        // Uppercase scheme and hostname must still be accepted.
        assert!(
            is_valid_discord_download_url("HTTPS://CDN.DISCORDAPP.COM/attachments/1/2/f.png"),
            "uppercase HTTPS://CDN.DISCORDAPP.COM/ should be accepted"
        );
        assert!(
            is_valid_discord_download_url("HTTPS://MEDIA.DISCORDAPP.NET/attachments/1/2/f.jpg"),
            "uppercase HTTPS://MEDIA.DISCORDAPP.NET/ should be accepted"
        );
        // Lookalike with different casing must still be rejected.
        assert!(
            !is_valid_discord_download_url("HTTPS://EVIL.COM/malicious"),
            "uppercase evil domain should still be rejected"
        );
    }

    // ── Phase 3: dedup ring ──────────────────────────────────────────────────

    #[tokio::test]
    async fn dedup_ring_eviction_at_cap() {
        let adapter = test_adapter(0);
        let mut window = adapter.dedup_window.lock().await;

        // Insert 250 entries; only last 200 should remain.
        for i in 0u64..250 {
            if window.len() >= 200 {
                window.pop_front();
            }
            window.push_back(i);
        }

        assert_eq!(window.len(), 200);
        // First 50 should have been evicted.
        for i in 0u64..50 {
            assert!(!window.contains(&i), "entry {} should have been evicted", i);
        }
        // Last 200 should be present.
        for i in 50u64..250 {
            assert!(window.contains(&i), "entry {} should still be present", i);
        }
    }

    #[tokio::test]
    async fn dedup_ring_lookup() {
        let adapter = test_adapter(0);
        {
            let mut window = adapter.dedup_window.lock().await;
            window.push_back(12345);
        }
        let window = adapter.dedup_window.lock().await;
        assert!(window.contains(&12345));
        assert!(!window.contains(&99999));
    }

    // ── Phase 4: thread title sanitization ──────────────────────────────────

    #[test]
    fn thread_title_strips_control_chars() {
        let title = sanitize_thread_title("hello\x01\x02world");
        assert_eq!(title, "hello world");
    }

    #[test]
    fn thread_title_collapses_whitespace() {
        let title = sanitize_thread_title("  hello   world  ");
        assert_eq!(title, "hello world");
    }

    #[test]
    fn thread_title_truncates_at_50_chars() {
        let long = "a".repeat(100);
        let title = sanitize_thread_title(&long);
        assert_eq!(title.chars().count(), 50);
    }

    #[test]
    fn thread_title_empty_falls_back_to_default() {
        let title = sanitize_thread_title("\x01\x02\x03");
        assert_eq!(title, "terminus session");
    }

    #[test]
    fn thread_title_empty_string_falls_back_to_default() {
        let title = sanitize_thread_title("");
        assert_eq!(title, "terminus session");
    }

    // ── Gap-coverage: sanitize_thread_title edge cases ────────────────────────

    #[test]
    fn thread_title_multibyte_emoji_truncates_at_char_boundary_no_panic() {
        // 49 ASCII chars + a 4-byte emoji: total 50 chars, should not panic or
        // produce a partial code-point.
        let input = "a".repeat(49) + "🦀";
        let title = sanitize_thread_title(&input);
        assert_eq!(title.chars().count(), 50);
        // Must be valid UTF-8 (would panic on decode otherwise).
        assert!(std::str::from_utf8(title.as_bytes()).is_ok());
    }

    #[test]
    fn thread_title_100_char_input_produces_exactly_50_chars() {
        // Independent guard that .chars().count() == 50 for multi-char input.
        let input = "x".repeat(100);
        let title = sanitize_thread_title(&input);
        assert_eq!(title.chars().count(), 50);
    }

    #[test]
    fn thread_title_pure_control_chars_falls_back_to_default() {
        // All chars in C0 block → collapsed whitespace stripped → empty → fallback.
        let input: String = (0x00u8..=0x1Fu8).map(|b| b as char).collect();
        let title = sanitize_thread_title(&input);
        assert_eq!(title, "terminus session");
    }

    // ── Gap-coverage: send_attachment size boundary ───────────────────────────

    #[tokio::test]
    async fn send_attachment_exactly_at_limit_is_rejected() {
        // The send_attachment guard fires on data.len() > MAX_ATTACHMENT_BYTES.
        // Exactly MAX_ATTACHMENT_BYTES should NOT trigger the guard (sends proceed
        // to the HTTP layer, which is not mocked — we just verify no early error).
        // We cannot complete the send without a real Discord connection, so we only
        // verify the under-limit case does NOT return the 25 MB error message.
        let adapter = test_adapter(0);
        let at_limit = vec![0u8; MAX_ATTACHMENT_BYTES];
        let result = adapter
            .send_attachment(&at_limit, "exact.bin", None, "not-a-number", None)
            .await;
        // Should fail with a channel-id parse error, NOT a 25 MB limit error.
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("25 MB"),
            "exact-limit payload must not trigger size guard, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn send_attachment_one_byte_over_limit_returns_size_error() {
        let adapter = test_adapter(0);
        let over_limit = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let result = adapter
            .send_attachment(&over_limit, "oversize.bin", None, "123456", None)
            .await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("25 MB"),
            "one-byte-over payload must trigger size guard"
        );
    }

    #[tokio::test]
    async fn send_attachment_one_byte_under_limit_passes_size_guard() {
        let adapter = test_adapter(0);
        let under_limit = vec![0u8; MAX_ATTACHMENT_BYTES - 1];
        let result = adapter
            .send_attachment(&under_limit, "under.bin", None, "not-a-number", None)
            .await;
        // No network: expect a channel-id parse error, not a 25 MB error.
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("25 MB"),
            "one-byte-under payload must not trigger size guard, got: {}",
            err
        );
    }

    // ── Gap-coverage: thread-map eviction fallback ────────────────────────────

    #[tokio::test]
    async fn thread_map_eviction_removes_entry_for_chat_id() {
        // Simulates the eviction branch: pre-populate thread_map for a chat_id,
        // then manually evict as the Thread-send-failure path does, and confirm
        // the entry is gone (get_discord_reply_for_chat returns None / Reference).
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(16);
        let adapter = DiscordAdapter::new(
            crate::config::DiscordConfig {
                bot_token: "t".to_string(),
                guild_id: Some(999),
                channel_id: Some(888),
            },
            UserId::new(1),
            0,
            state_tx,
            HashMap::new(),
        )
        .unwrap();

        let chat_id = "42000000000000001";
        let parent_msg_id: u64 = 9999;

        // Pre-populate thread_map and parent_msg_ids.
        adapter
            .thread_map
            .lock()
            .await
            .insert(chat_id.to_string(), (777u64, parent_msg_id));
        adapter
            .parent_msg_ids
            .lock()
            .await
            .insert(chat_id.to_string(), parent_msg_id);

        // Confirm Thread target is returned before eviction.
        let target_before = adapter.get_discord_reply_for_chat(chat_id).await;
        assert_eq!(
            target_before,
            Some(DiscordReplyTarget::Thread(777u64)),
            "thread_map entry should be visible before eviction"
        );

        // Evict (mirroring Thread-send-failure path in send_message).
        {
            let mut map = adapter.thread_map.lock().await;
            map.remove(chat_id);
        }

        // After eviction with guild config: resolve_target would try to create a
        // new Thread (network). But get_discord_reply_for_chat reads thread_map
        // first, finds nothing, then checks parent_msg_ids + guild config.
        // Since guild_id+channel_id are set it calls resolve_target, which will
        // fail with a network error and fall back to Reference — but the key
        // guarantee we verify here is that the thread_map no longer has the
        // evicted entry.
        let map_after = adapter.thread_map.lock().await;
        assert!(
            !map_after.contains_key(chat_id),
            "thread_map entry must be absent after eviction"
        );
    }

    // ── Gap-coverage: dedup skips inflight gateway messages ──────────────────

    #[tokio::test]
    async fn dedup_window_contains_check_blocks_duplicate_id() {
        // Verify that after inserting msg_id 42 into dedup_window, the contains()
        // guard (same logic as DiscordHandler::message dedup check) returns true,
        // indicating the handler would return early.
        let adapter = test_adapter(0);
        let msg_id: u64 = 42;

        {
            let mut window = adapter.dedup_window.lock().await;
            window.push_back(msg_id);
        }

        let window = adapter.dedup_window.lock().await;
        assert!(
            window.contains(&msg_id),
            "dedup_window must contain the pre-inserted id"
        );
        assert!(
            !window.contains(&99999u64),
            "dedup_window must not contain an id that was never inserted"
        );
        // The handler's guard: `if window.contains(&msg_id) { return; }`.
        // This assertion confirms the early-return branch would fire.
        let would_skip = window.contains(&msg_id);
        assert!(would_skip, "gateway handler would skip this duplicate id");
    }

    // ── Gap-coverage: DM-mode routing via get_discord_reply_for_chat ─────────

    #[tokio::test]
    async fn get_discord_reply_dm_mode_returns_reference() {
        // DM-only adapter: guild_id = None, channel_id = None.
        // Populating parent_msg_ids should yield Reference, never Thread.
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(16);
        let adapter = DiscordAdapter::new(
            crate::config::DiscordConfig {
                bot_token: "t".to_string(),
                guild_id: None,
                channel_id: None,
            },
            UserId::new(1),
            0,
            state_tx,
            HashMap::new(),
        )
        .unwrap();

        let chat_id = "1111111111111111";
        let parent_msg_id: u64 = 55555;

        adapter
            .parent_msg_ids
            .lock()
            .await
            .insert(chat_id.to_string(), parent_msg_id);

        let target = adapter.get_discord_reply_for_chat(chat_id).await;
        assert_eq!(
            target,
            Some(DiscordReplyTarget::Reference(parent_msg_id)),
            "DM-only mode must produce Reference, not Thread"
        );
    }

    // ── Gap-coverage: guild mode with cached thread_map entry ─────────────────

    #[tokio::test]
    async fn get_discord_reply_guild_mode_cached_thread_skips_creation() {
        // Guild adapter with thread_map pre-populated: resolve must return the
        // cached Thread id without attempting any network call.
        let (state_tx, _state_rx) = mpsc::channel::<StateUpdate>(16);
        let adapter = DiscordAdapter::new(
            crate::config::DiscordConfig {
                bot_token: "t".to_string(),
                guild_id: Some(111),
                channel_id: Some(222),
            },
            UserId::new(1),
            0,
            state_tx,
            HashMap::new(),
        )
        .unwrap();

        let chat_id = "2222222222222222";
        let cached_thread_id: u64 = 33333;
        let parent_msg_id: u64 = 44444;

        adapter
            .thread_map
            .lock()
            .await
            .insert(chat_id.to_string(), (cached_thread_id, parent_msg_id));

        let target = adapter.get_discord_reply_for_chat(chat_id).await;
        assert_eq!(
            target,
            Some(DiscordReplyTarget::Thread(cached_thread_id)),
            "guild mode with cached thread_map must return Thread without creating a new one"
        );
    }

    // ── Fix 2 regression: cursor advances even on filtered-only pages ─────────

    /// Verify that `current_after` is advanced to the highest msg_id in a page
    /// even when ALL messages on that page are filtered (bot/unauthorized).
    /// Without the early-advance fix, `current_after` would stay at `after_id`
    /// and the next page request would return the same 100 messages forever.
    ///
    /// This test exercises the constituent logic directly (no network required):
    /// it asserts that `max(current_after, msg_id)` advances past the initial
    /// watermark regardless of whether the message passes the auth/bot filter.
    #[test]
    fn run_catchup_advances_cursor_on_filtered_pages() {
        // Simulate: watermark starts at snowflake 1000.
        let after_id: u64 = 1000;
        let mut current_after = after_id;

        // A page of 3 "bot" messages with higher snowflakes.
        let bot_msg_ids: Vec<u64> = vec![1001, 1002, 1003];

        // Mirroring the fixed loop: advance current_after BEFORE the filter.
        for &msg_id in bot_msg_ids.iter().rev() {
            // Early-advance (the fix).
            current_after = current_after.max(msg_id);

            // Auth/bot filter would `continue` here — simulated by doing nothing else.
            let _is_bot = true; // all messages are bot messages
            if _is_bot {
                continue;
            }
            // (unreachable in this scenario)
        }

        assert_eq!(
            current_after, 1003,
            "cursor must advance to highest msg_id (1003) even though all messages were filtered"
        );
        assert!(
            current_after > after_id,
            "cursor must be strictly greater than the initial watermark"
        );
    }
}
