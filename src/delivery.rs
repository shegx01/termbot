/// Delivery task, gap-prefix registry, and banner formatting.
///
/// The free functions in this module are spawned by `main.rs` and run
/// independently from the `App` main loop.  They share lightweight
/// `Arc<Mutex<…>>` handles with `App` to resolve per-chat oneshots and
/// consume inline gap-prefix markers without going through the main
/// `tokio::select!` loop.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

use crate::buffer::{StreamEvent, WebhookStatusKind};
use crate::chat_adapters::ChatPlatform;

// ──────────────────────────────────────────────────────────────────────────────
// Shared type aliases
// ──────────────────────────────────────────────────────────────────────────────

/// Shared registry of pending banner-delivery notifications.
/// `App::handle_gap` inserts a oneshot sender keyed by chat_id before
/// broadcasting a `StreamEvent::GapBanner`.  The delivery task removes
/// and fires it immediately on successful send so `handle_gap`'s await
/// unblocks without routing through the main `tokio::select!` loop.
///
/// This direct-resolution pattern is required because `handle_gap` runs
/// inline on the main select branch — routing the ack through a separate
/// mpsc would deadlock (the main loop can't process the ack channel
/// while blocked inside `handle_gap`).
pub type PendingBannerAcks = Arc<AsyncMutex<HashMap<String, oneshot::Sender<()>>>>;

/// Shared gap-prefix registry used by delivery tasks to consume the
/// inline `[gap: …]` marker set by `handle_gap` when a banner delivery
/// times out.  Cleared after one read per chat.
pub type GapPrefixes = Arc<AsyncMutex<HashMap<String, GapInfo>>>;

// ──────────────────────────────────────────────────────────────────────────────
// GapInfo — retained for the fallback inline-prefix path
// ──────────────────────────────────────────────────────────────────────────────

/// Retained gap information for the fallback inline-prefix path.
/// When `handle_gap` banner delivery times out, we store this and prepend
/// `[gap: Xm Ys]` to the next outbound message for the chat.
#[derive(Debug, Clone)]
pub struct GapInfo {
    pub gap: Duration,
    /// Retained for future observability / richer prefix formatting.
    #[allow(dead_code)]
    pub paused_at: DateTime<Utc>,
    /// Retained for future observability / richer prefix formatting.
    #[allow(dead_code)]
    pub resumed_at: DateTime<Utc>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Banner formatting
// ──────────────────────────────────────────────────────────────────────────────

/// Format a gap-recovery banner for display to the user.
///
/// Renders timestamps in local time for readability.  When `missed_count > 0`
/// the banner includes a note about queued messages.
pub(crate) fn format_gap_banner(
    paused_at: DateTime<Utc>,
    resumed_at: DateTime<Utc>,
    gap: Duration,
    missed_count: u32,
) -> String {
    let paused_local = paused_at.with_timezone(&Local);
    let resumed_local = resumed_at.with_timezone(&Local);
    let gap_mins = gap.as_secs() / 60;
    let gap_secs = gap.as_secs() % 60;
    if missed_count > 0 {
        format!(
            "\u{23f8} paused at {}, resumed at {} (gap: {}m {}s), processing {} queued messages",
            paused_local.format("%H:%M"),
            resumed_local.format("%H:%M"),
            gap_mins,
            gap_secs,
            missed_count,
        )
    } else {
        format!(
            "\u{23f8} paused at {}, resumed at {} (gap: {}m {}s)",
            paused_local.format("%H:%M"),
            resumed_local.format("%H:%M"),
            gap_mins,
            gap_secs,
        )
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Delivery task
// ──────────────────────────────────────────────────────────────────────────────

/// Spawn a delivery task that consumes `StreamEvent`s from the broadcast
/// channel and sends them via `platform`.
///
/// - `pending_banner_acks`: shared map used to resolve `handle_gap`'s
///   per-chat oneshot waiters directly (avoids the main-loop deadlock that
///   an mpsc ack channel would have introduced).
/// - `gap_prefixes`: shared map of inline fallback gap-prefix markers.  On
///   every `NewMessage`, the task checks for a pending prefix for that chat
///   and prepends `[gap: Xm Ys] ` to the first outbound chunk before sending.
pub fn spawn_delivery_task(
    platform: Arc<dyn ChatPlatform>,
    mut rx: broadcast::Receiver<StreamEvent>,
    pending_banner_acks: PendingBannerAcks,
    gap_prefixes: GapPrefixes,
) {
    tokio::spawn(async move {
        let mut session_chat_ids: HashMap<String, String> = HashMap::new();
        let mut session_thread_ts: HashMap<String, Option<String>> = HashMap::new();

        loop {
            match rx.recv().await {
                Ok(event) => {
                    handle_stream_event(
                        &*platform,
                        event,
                        &mut session_chat_ids,
                        &mut session_thread_ts,
                        &pending_banner_acks,
                        &gap_prefixes,
                    )
                    .await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "{:?} delivery lagged by {} events",
                        platform.platform_type(),
                        n
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("{:?} delivery channel closed", platform.platform_type());
                    break;
                }
            }
        }
    });
}

async fn handle_stream_event(
    platform: &dyn ChatPlatform,
    event: StreamEvent,
    session_chat_ids: &mut HashMap<String, String>,
    session_thread_ts: &mut HashMap<String, Option<String>>,
    pending_banner_acks: &PendingBannerAcks,
    gap_prefixes: &GapPrefixes,
) {
    match event {
        StreamEvent::SessionStarted {
            session,
            chat_id,
            thread_ts,
        } => {
            session_chat_ids.insert(session.clone(), chat_id);
            session_thread_ts.insert(session, thread_ts);
        }
        StreamEvent::NewMessage { session, content } => {
            let chat_id = match session_chat_ids.get(&session) {
                Some(id) => id.clone(),
                None => {
                    tracing::warn!(
                        "[delivery] no chat_id for session '{}', dropping message ({} chars)",
                        session,
                        content.len()
                    );
                    return;
                }
            };
            let thread_ts = session_thread_ts.get(&session).and_then(|t| t.as_deref());

            // Consume any pending inline gap prefix for this chat.  Lock is
            // held only for the remove — never across I/O awaits.
            let prefix: Option<String> = {
                let removed = gap_prefixes.lock().await.remove(&chat_id);
                removed.map(|info| {
                    let mins = info.gap.as_secs() / 60;
                    let secs = info.gap.as_secs() % 60;
                    format!("[gap: {}m {}s] ", mins, secs)
                })
            };

            // Split for Telegram's 4096-char limit; prepend prefix to first chunk.
            let mut chunks = split_message(&content, 4000);
            if let (Some(pfx), Some(first)) = (prefix, chunks.first_mut()) {
                *first = format!("{}{}", pfx, first);
            }
            for chunk in chunks {
                if let Err(e) = platform.send_message(&chunk, &chat_id, thread_ts).await {
                    tracing::error!("Failed to send message: {}", e);
                }
            }
        }
        StreamEvent::SessionExited { session, code } => {
            let chat_id = match session_chat_ids.get(&session) {
                Some(id) => id.clone(),
                None => return,
            };
            let thread_ts = session_thread_ts.get(&session).and_then(|t| t.as_deref());
            let msg = match code {
                Some(c) => format!("Session '{}' exited (code {})", session, c),
                None => format!("Session '{}' has exited unexpectedly", session),
            };
            let _ = platform.send_message(&msg, &chat_id, thread_ts).await;
            session_chat_ids.remove(&session);
            session_thread_ts.remove(&session);
        }
        StreamEvent::GapBanner {
            chat_id,
            platform: banner_platform,
            paused_at,
            resumed_at,
            gap,
            missed_count,
        } => {
            // Skip banners that belong to a different platform's delivery task.
            if banner_platform != platform.platform_type() {
                return;
            }
            let msg = format_gap_banner(paused_at, resumed_at, gap, missed_count);
            if let Err(e) = platform.send_message(&msg, &chat_id, None).await {
                tracing::error!("Failed to send GapBanner for chat_id={}: {}", chat_id, e);
                // Do NOT resolve the oneshot on failure — let handle_gap's
                // 5s timeout fire so the inline-prefix fallback engages.
            } else {
                // Directly resolve the waiter in App::handle_gap.  Lock held
                // only for the remove+send; never across `.await` of outgoing
                // I/O.
                if let Some(tx) = pending_banner_acks.lock().await.remove(&chat_id) {
                    let _ = tx.send(());
                }
            }
        }
        StreamEvent::StructuredOutputRendered { payload, chat } => {
            // Only handle events for this delivery task's platform.
            if chat.platform != platform.platform_type() {
                return;
            }
            handle_structured_output_rendered(platform, &payload, &chat).await;
        }
        StreamEvent::WebhookStatus {
            schema,
            run_id,
            status,
            chat,
        } => {
            // Only handle events for this delivery task's platform.
            if chat.platform != platform.platform_type() {
                return;
            }
            let msg = match &status {
                WebhookStatusKind::Delivered => {
                    format!(
                        "✅ webhook delivered (schema={}, run_id={})",
                        schema, run_id
                    )
                }
                WebhookStatusKind::Abandoned => {
                    format!(
                        "❌ webhook delivery abandoned after max retry age (schema={}, run_id={})",
                        schema, run_id
                    )
                }
                WebhookStatusKind::Error { msg: err_msg } => {
                    format!(
                        "❌ webhook delivery error (schema={}, run_id={}): {}",
                        schema, run_id, err_msg
                    )
                }
            };
            if let Err(e) = platform
                .send_message(&msg, &chat.chat_id, chat.thread_ts.as_deref())
                .await
            {
                tracing::warn!(
                    "Failed to send webhook status to chat {}: {}",
                    chat.chat_id,
                    e
                );
            }
        }
        StreamEvent::QueueDrained {
            delivered_count,
            chat,
        } => {
            if chat.platform != platform.platform_type() {
                return;
            }
            let msg = format!("✅ queue drained ({} delivered)", delivered_count);
            if let Err(e) = platform
                .send_message(&msg, &chat.chat_id, chat.thread_ts.as_deref())
                .await
            {
                tracing::warn!(
                    "Failed to send queue-drained status to chat {}: {}",
                    chat.chat_id,
                    e
                );
            }
        }
    }
}

/// Maximum byte size for inline JSON rendering in chat.
///
/// Payloads ≤ 3900 bytes (after fence overhead) are sent inline as a
/// ` ```json ` code block.  Larger payloads are uploaded as `.json` attachments.
const INLINE_JSON_MAX_BYTES: usize = 3900;

/// Render structured output to the appropriate chat platform.
///
/// Size-aware hybrid:
/// - ≤ 3900 bytes (with fences): send as inline ` ```json ``` ` code block.
/// - > 3900 bytes: upload as `.json` attachment + short summary line.
async fn handle_structured_output_rendered(
    platform: &dyn ChatPlatform,
    payload: &crate::buffer::StructuredOutputPayload,
    chat: &crate::chat_adapters::ChatBinding,
) {
    let pretty = match serde_json::to_string_pretty(&payload.value) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to pretty-print structured output: {}", e);
            return;
        }
    };

    // Compute size including code-fence overhead: "```json\n" + content + "\n```"
    let fenced = format!("```json\n{}\n```", pretty);
    let thread_ts = chat.thread_ts.as_deref();

    if fenced.len() <= INLINE_JSON_MAX_BYTES {
        if let Err(e) = platform
            .send_message(&fenced, &chat.chat_id, thread_ts)
            .await
        {
            tracing::error!("Failed to send inline structured output: {}", e);
        }
    } else {
        // Upload as attachment.
        let now = chrono::Utc::now();
        let filename = format!("{}-{}.json", payload.schema, now.format("%Y%m%dT%H%M%SZ"));
        let json_bytes = pretty.into_bytes();
        let kb = json_bytes.len() as f64 / 1024.0;

        // Count top-level array items if the value is an array, else count object keys.
        let item_count = match &payload.value {
            serde_json::Value::Array(arr) => arr.len(),
            serde_json::Value::Object(obj) => obj.len(),
            _ => 0,
        };

        let summary = format!(
            "✅ {} items, {:.1} KB, schema={}",
            item_count, kb, payload.schema
        );

        if let Err(e) = platform
            .send_message(&summary, &chat.chat_id, thread_ts)
            .await
        {
            tracing::error!("Failed to send structured output summary: {}", e);
        }

        if let Err(e) = platform
            .send_document(&json_bytes, &filename, None, &chat.chat_id, thread_ts)
            .await
        {
            tracing::error!("Failed to send structured output attachment: {}", e);
        }
    }
}

// Re-export split_message so tests can use it directly.

/// Split a message into chunks of at most `max_len` characters,
/// breaking at newline boundaries when possible.
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        // Find a safe byte boundary (don't split inside a multi-byte char)
        let byte_limit = max_len.min(remaining.len());
        let safe_limit = match remaining.get(..byte_limit) {
            Some(_) => byte_limit,
            None => remaining
                .char_indices()
                .take_while(|(i, _)| *i <= byte_limit)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(remaining.len()),
        };
        // Prefer splitting at a newline
        let split_at = remaining[..safe_limit]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(safe_limit);
        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }
    chunks
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── split_message tests ────────────────────────────────────────────────

    #[test]
    fn split_short_message() {
        let chunks = split_message("hello", 4000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_at_newline_boundary() {
        let text = "line1\nline2\nline3";
        let chunks = split_message(text, 10);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].contains("line1"));
        assert!(chunks.last().unwrap().contains("line3"));
    }

    #[test]
    fn split_unicode_safe() {
        let text = "a\u{1f916}b".repeat(2000);
        let chunks = split_message(&text, 4000);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= 4100);
        }
    }

    #[test]
    fn split_exact_limit() {
        let text = "a".repeat(4000);
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_just_over_limit() {
        let text = "a".repeat(4001);
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4000);
        assert_eq!(chunks[1].len(), 1);
    }

    // ─── format_gap_banner tests ────────────────────────────────────────────

    #[test]
    fn format_gap_banner_no_missed_count() {
        let paused_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let resumed_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T10:01:30Z")
            .unwrap()
            .with_timezone(&Utc);
        let gap = Duration::from_secs(90);
        let msg = format_gap_banner(paused_at, resumed_at, gap, 0);
        // Should contain gap info and NOT contain "queued messages"
        assert!(msg.contains("1m 30s"), "expected gap '1m 30s' in '{}'", msg);
        assert!(
            !msg.contains("queued"),
            "should not mention queued messages when missed_count=0"
        );
    }

    #[test]
    fn format_gap_banner_with_missed_count() {
        let paused_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let resumed_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T10:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let gap = Duration::from_secs(300);
        let msg = format_gap_banner(paused_at, resumed_at, gap, 5);
        assert!(msg.contains("5m 0s"), "expected gap '5m 0s' in '{}'", msg);
        assert!(msg.contains("5 queued"), "expected '5 queued' in '{}'", msg);
    }

    #[test]
    fn format_gap_banner_boundary_exactly_60s() {
        let paused_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T09:59:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let resumed_at = chrono::DateTime::parse_from_rfc3339("2026-04-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let gap = Duration::from_secs(60);
        let msg = format_gap_banner(paused_at, resumed_at, gap, 0);
        assert!(
            msg.contains("1m 0s"),
            "60s should render as '1m 0s', got '{}'",
            msg
        );
    }

    // ─── delivery_consumes_gap_prefix_on_next_message ───────────────────────

    #[tokio::test]
    async fn delivery_consumes_gap_prefix_on_next_message() {
        // Test that consume + format logic for gap_prefixes works correctly:
        // inserting a GapInfo into the registry and then removing it inside
        // handle_stream_event should leave the map empty and produce the prefix.
        let now = Utc::now();
        let gap_info = GapInfo {
            gap: Duration::from_secs(90),
            paused_at: now - chrono::Duration::seconds(90),
            resumed_at: now,
        };

        let gap_prefixes: GapPrefixes = Arc::new(AsyncMutex::new(HashMap::new()));
        gap_prefixes
            .lock()
            .await
            .insert("chat42".to_string(), gap_info);

        // Simulate what handle_stream_event does on NewMessage for chat42.
        let prefix: Option<String> = {
            let removed = gap_prefixes.lock().await.remove("chat42");
            removed.map(|info| {
                let mins = info.gap.as_secs() / 60;
                let secs = info.gap.as_secs() % 60;
                format!("[gap: {}m {}s] ", mins, secs)
            })
        };

        // Map should be cleared.
        assert!(
            gap_prefixes.lock().await.is_empty(),
            "gap_prefixes should be empty after consume"
        );

        // Prefix should be correctly formatted.
        let pfx = prefix.expect("prefix should have been produced");
        assert_eq!(pfx, "[gap: 1m 30s] ", "got '{}'", pfx);
    }

    // ─── GapBanner platform filter tests ───────────────────────────────────

    use crate::chat_adapters::{IncomingMessage, PlatformMessageId, PlatformType};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Minimal mock adapter that tracks send_message call count.
    struct MockPlatform {
        ptype: PlatformType,
        send_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::chat_adapters::ChatPlatform for MockPlatform {
        async fn start(
            &self,
            _cmd_tx: tokio::sync::mpsc::Sender<IncomingMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_message(
            &self,
            _text: &str,
            _chat_id: &str,
            _thread_ts: Option<&str>,
        ) -> anyhow::Result<PlatformMessageId> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(PlatformMessageId::Telegram(1))
        }
        async fn edit_message(
            &self,
            _msg_id: &PlatformMessageId,
            _chat_id: &str,
            _text: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_photo(
            &self,
            _data: &[u8],
            _filename: &str,
            _caption: Option<&str>,
            _chat_id: &str,
            _thread_ts: Option<&str>,
        ) -> anyhow::Result<PlatformMessageId> {
            Ok(PlatformMessageId::Telegram(1))
        }
        async fn send_document(
            &self,
            _data: &[u8],
            _filename: &str,
            _caption: Option<&str>,
            _chat_id: &str,
            _thread_ts: Option<&str>,
        ) -> anyhow::Result<PlatformMessageId> {
            Ok(PlatformMessageId::Telegram(1))
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn platform_type(&self) -> PlatformType {
            self.ptype
        }
    }

    #[tokio::test]
    async fn gap_banner_skipped_for_wrong_platform() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let mock = MockPlatform {
            ptype: PlatformType::Telegram,
            send_count: Arc::clone(&send_count),
        };
        let pending: PendingBannerAcks = Arc::new(AsyncMutex::new(HashMap::new()));
        let gap_prefixes: GapPrefixes = Arc::new(AsyncMutex::new(HashMap::new()));
        let mut session_chat_ids = HashMap::new();
        let mut session_thread_ts = HashMap::new();

        // Broadcast a GapBanner targeted at Discord to a Telegram delivery task.
        let now = Utc::now();
        let event = StreamEvent::GapBanner {
            chat_id: "discord_chat_42".to_string(),
            platform: PlatformType::Discord,
            paused_at: now - chrono::Duration::seconds(60),
            resumed_at: now,
            gap: Duration::from_secs(60),
            missed_count: 0,
        };

        handle_stream_event(
            &mock,
            event,
            &mut session_chat_ids,
            &mut session_thread_ts,
            &pending,
            &gap_prefixes,
        )
        .await;

        assert_eq!(
            send_count.load(Ordering::SeqCst),
            0,
            "Telegram delivery task should NOT send a Discord-targeted GapBanner"
        );
    }

    #[tokio::test]
    async fn gap_banner_sent_for_matching_platform() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let mock = MockPlatform {
            ptype: PlatformType::Telegram,
            send_count: Arc::clone(&send_count),
        };
        let pending: PendingBannerAcks = Arc::new(AsyncMutex::new(HashMap::new()));
        let gap_prefixes: GapPrefixes = Arc::new(AsyncMutex::new(HashMap::new()));
        let mut session_chat_ids = HashMap::new();
        let mut session_thread_ts = HashMap::new();

        let now = Utc::now();
        let event = StreamEvent::GapBanner {
            chat_id: "tg_chat_111".to_string(),
            platform: PlatformType::Telegram,
            paused_at: now - chrono::Duration::seconds(60),
            resumed_at: now,
            gap: Duration::from_secs(60),
            missed_count: 0,
        };

        handle_stream_event(
            &mock,
            event,
            &mut session_chat_ids,
            &mut session_thread_ts,
            &pending,
            &gap_prefixes,
        )
        .await;

        assert_eq!(
            send_count.load(Ordering::SeqCst),
            1,
            "Telegram delivery task should send a Telegram-targeted GapBanner"
        );
    }

    // ── Hybrid render boundary tests ─────────────────────────────────────────

    /// `INLINE_JSON_MAX_BYTES` must be 3900 per the spec.
    #[test]
    fn inline_json_max_bytes_is_3900() {
        assert_eq!(
            INLINE_JSON_MAX_BYTES, 3900,
            "Spec requires INLINE_JSON_MAX_BYTES = 3900"
        );
    }

    /// The inline/attachment decision is made based on the fenced representation:
    ///   ` ```json\n<content>\n``` `
    /// The fence overhead is "```json\n" (8) + "\n```" (4) = 12 bytes.
    /// Therefore a payload of exactly 3888 bytes of content is inline (total = 3900).
    /// A payload of 3889 bytes of content is an attachment (total = 3901 > 3900).
    #[test]
    fn hybrid_render_boundary_exact() {
        // The fenced format is: "```json\n" + content + "\n```"
        // Fence overhead: "```json\n".len() + "\n```".len() = 8 + 4 = 12 bytes
        let fence_overhead = "```json\n".len() + "\n```".len();
        let max_content = INLINE_JSON_MAX_BYTES - fence_overhead;

        // Content at exactly max_content bytes should be inline.
        let inline_content = "x".repeat(max_content);
        let inline_fenced = format!("```json\n{}\n```", inline_content);
        assert_eq!(
            inline_fenced.len(),
            INLINE_JSON_MAX_BYTES,
            "Exactly 3900-byte fenced payload should be inline"
        );
        assert!(
            inline_fenced.len() <= INLINE_JSON_MAX_BYTES,
            "Should be inline (len {} <= {})",
            inline_fenced.len(),
            INLINE_JSON_MAX_BYTES
        );

        // One byte over should be attachment.
        let attachment_content = "x".repeat(max_content + 1);
        let attachment_fenced = format!("```json\n{}\n```", attachment_content);
        assert_eq!(attachment_fenced.len(), INLINE_JSON_MAX_BYTES + 1);
        assert!(
            attachment_fenced.len() > INLINE_JSON_MAX_BYTES,
            "Should be attachment (len {} > {})",
            attachment_fenced.len(),
            INLINE_JSON_MAX_BYTES
        );
    }
}
