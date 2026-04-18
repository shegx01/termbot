pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;

use crate::buffer::{StreamEvent, StructuredOutputPayload};
use crate::chat_adapters::{Attachment, ChatBinding, ChatPlatform, PlatformType, ReplyContext};
use crate::command::HarnessOptions;
use crate::structured_output::{DeliveryJob, DeliveryQueue, SchemaRegistry, WebhookClient};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessKind {
    Claude,
    Gemini,
    Codex,
    Opencode,
}

impl HarnessKind {
    /// Parse a harness name from user input (case-insensitive).
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "gemini" => Some(Self::Gemini),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::Codex => "Codex",
            Self::Opencode => "OpenCode",
        }
    }
}

/// Build a prefixed session key for storing a named harness session.
///
/// Keys are `{kind}:{name}` where kind is the lowercase harness identifier.
/// This prevents cross-harness name collisions (e.g. `claude --name foo` and
/// `opencode --name foo` can coexist as `claude:foo` and `opencode:foo`).
pub fn build_session_key(kind: HarnessKind, name: &str) -> String {
    let kind_prefix = match kind {
        HarnessKind::Claude => "claude",
        HarnessKind::Gemini => "gemini",
        HarnessKind::Codex => "codex",
        HarnessKind::Opencode => "opencode",
    };
    format!("{}:{}", kind_prefix, name)
}

/// Events streamed from a harness session to the chat delivery layer.
/// ToolUse is optional — non-Claude harnesses may only emit Text, Done, Error.
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    /// Harness is using a tool (Read, Write, Edit, Bash, etc.)
    ///
    /// `input`/`output` are populated by harnesses that expose structured
    /// tool IO (e.g. opencode); other harnesses pass `None` and rely on
    /// `description` for a human-readable preview.
    ToolUse {
        tool: String,
        description: String,
        input: Option<String>,
        output: Option<String>,
    },
    /// Text from the harness response
    Text(String),
    /// A file produced by the harness (image, document, data file, etc.)
    File {
        data: Vec<u8>,
        media_type: String,
        filename: String,
    },
    /// Session completed
    Done { session_id: String },
    /// An error occurred
    Error(String),
    /// Validated structured output produced by the Claude SDK.
    ///
    /// Emitted when `ClaudeAgentOptions.output_format` was set and the SDK
    /// successfully returned a `structured_output` in the `ResultMessage`.
    StructuredOutput {
        /// Schema name (from `HarnessOptions.schema`).
        schema: String,
        /// Validated `serde_json::Value` matching the configured schema.
        value: serde_json::Value,
        /// ULID run ID (used as queue filename and `X-Terminus-Run-Id` header).
        run_id: String,
    },
}

#[async_trait]
pub trait Harness: Send + Sync {
    #[allow(dead_code)]
    fn kind(&self) -> HarnessKind;
    fn supports_resume(&self) -> bool;

    /// Run a prompt, returning a channel that streams HarnessEvents.
    /// Implementations that spawn background tasks MUST catch panics internally
    /// and send HarnessEvent::Error before the channel closes.
    async fn run_prompt(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        cwd: &Path,
        session_id: Option<&str>,
        options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>>;

    /// Get stored session ID for multi-turn resume.
    fn get_session_id(&self, session_name: &str) -> Option<String>;
    /// Store session ID after prompt completes. Uses interior mutability (Mutex).
    fn set_session_id(&self, session_name: &str, id: String);
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", &s[..end])
}

/// Format a tool use event for display in chat.
#[allow(dead_code)] // public API used in tests and future callers
pub fn format_tool_event(tool: &str, description: &str) -> String {
    format_tool_event_full(tool, description, None, None)
}

/// Format a tool use event with optional structured `input`/`output` fields.
///
/// When both `input` and `output` are `Some`, renders `tool(input) → output`
/// (with previews truncated). Otherwise falls back to `description`.
pub fn format_tool_event_full(
    tool: &str,
    description: &str,
    input: Option<&str>,
    output: Option<&str>,
) -> String {
    let icon = match tool {
        "Read" => "📖",
        "Write" => "📝",
        "Edit" => "✏️",
        "Bash" => "💻",
        "Glob" => "🔍",
        "Grep" => "🔎",
        "Agent" => "🤖",
        "WebSearch" | "WebFetch" => "🌐",
        "Thinking" => "🧠",
        _ => "🔧",
    };

    // Prefer structured input/output when both are present.
    if let (Some(inp), Some(outp)) = (input, output) {
        let inp_preview = truncate(inp, 40);
        let outp_preview = truncate(outp, 40);
        return format!("{} {}({}) → {}", icon, tool, inp_preview, outp_preview);
    }

    if description.is_empty() {
        format!("{} {}", icon, tool)
    } else {
        format!("{} {} {}", icon, tool, truncate(description, 80))
    }
}

/// Read-only context passed into `drive_harness`.
///
/// Groups the reply context, platform references, and structured-output
/// infrastructure so the parameter list stays bounded as new concerns are added.
pub struct HarnessContext<'a> {
    pub ctx: &'a ReplyContext,
    pub telegram: Option<&'a dyn ChatPlatform>,
    pub slack: Option<&'a dyn ChatPlatform>,
    pub discord: Option<&'a dyn ChatPlatform>,
    /// Registry for resolving schema names to values + webhook info.
    pub schema_registry: &'a SchemaRegistry,
    /// Durable delivery queue (write-ahead before webhook attempt).
    pub delivery_queue: &'a DeliveryQueue,
    /// HTTP client for sync webhook delivery.
    pub webhook_client: &'a WebhookClient,
    /// Broadcast sender for structured output chat rendering and status events.
    pub stream_tx: &'a tokio::sync::broadcast::Sender<StreamEvent>,
}

/// Consume HarnessEvents and deliver to chat. Handles tool-use deduplication,
/// batched flushing, text chunking, and error delivery.
/// Returns (got_any_output, session_id_from_done_event).
pub async fn drive_harness(
    mut event_rx: mpsc::Receiver<HarnessEvent>,
    hctx: &HarnessContext<'_>,
) -> (bool, Option<String>) {
    let ctx = hctx.ctx;
    let telegram = hctx.telegram;
    let slack = hctx.slack;
    let discord = hctx.discord;
    let mut tool_lines: Vec<String> = Vec::new();
    let mut last_tool_flush = tokio::time::Instant::now();
    let mut got_any_output = false;
    let mut last_tool_name: Option<String> = None;
    let mut consecutive_count: u32 = 0;
    let mut session_id_out: Option<String> = None;

    while let Some(event) = event_rx.recv().await {
        match event {
            HarnessEvent::ToolUse {
                tool,
                description,
                input,
                output,
            } => {
                got_any_output = true;

                if last_tool_name.as_deref() == Some(&tool) {
                    consecutive_count += 1;
                } else {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                    }
                    consecutive_count = 0;
                    last_tool_name = Some(tool.clone());
                    tool_lines.push(format_tool_event_full(
                        &tool,
                        &description,
                        input.as_deref(),
                        output.as_deref(),
                    ));
                }

                // Flush batch every 3s or every 5 distinct tool lines
                if !tool_lines.is_empty()
                    && (tool_lines.len() >= 5
                        || last_tool_flush.elapsed() > std::time::Duration::from_secs(3))
                {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                        consecutive_count = 0;
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                    tool_lines.clear();
                    last_tool_flush = tokio::time::Instant::now();
                }
            }
            HarnessEvent::Text(text) => {
                got_any_output = true;
                // Flush tool lines with trailing count
                if !tool_lines.is_empty() {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                        consecutive_count = 0;
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                    tool_lines.clear();
                    last_tool_name = None;
                }
                if !text.is_empty() {
                    let max_len = match ctx.platform {
                        crate::chat_adapters::PlatformType::Discord => 1900,
                        _ => 4000,
                    };
                    for chunk in split_message(&text, max_len) {
                        send_reply(ctx, &chunk, telegram, slack, discord).await;
                    }
                }
            }
            HarnessEvent::Done { session_id } => {
                if !tool_lines.is_empty() {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                }
                got_any_output = true;
                if !session_id.is_empty() {
                    session_id_out = Some(session_id);
                }
                break;
            }
            HarnessEvent::File {
                data,
                ref media_type,
                ref filename,
            } => {
                got_any_output = true;
                // Flush pending tool lines first
                if !tool_lines.is_empty() {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                        consecutive_count = 0;
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                    tool_lines.clear();
                    last_tool_name = None;
                }
                // Route images to send_photo (native preview), documents to send_document
                if media_type.starts_with("image/") {
                    send_photo_reply(ctx, &data, filename, None, telegram, slack, discord).await;
                } else {
                    send_document_reply(ctx, &data, filename, None, telegram, slack, discord).await;
                }
            }
            HarnessEvent::Error(e) => {
                if !tool_lines.is_empty() {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                }
                send_error(ctx, &e, telegram, slack, discord).await;
                got_any_output = true;
                break;
            }
            HarnessEvent::StructuredOutput {
                schema,
                value,
                run_id,
            } => {
                got_any_output = true;

                // Flush any pending tool lines first.
                if !tool_lines.is_empty() {
                    if consecutive_count > 0 {
                        if let Some(last) = tool_lines.last_mut() {
                            *last = format!("{} (+{} more)", last, consecutive_count);
                        }
                        consecutive_count = 0;
                    }
                    send_reply(ctx, &tool_lines.join("\n"), telegram, slack, discord).await;
                    tool_lines.clear();
                    last_tool_name = None;
                }

                let chat = ChatBinding::from(ctx);

                // Step 1: Broadcast for chat-side hybrid rendering.
                let _ = hctx.stream_tx.send(StreamEvent::StructuredOutputRendered {
                    payload: StructuredOutputPayload {
                        schema: schema.clone(),
                        value: value.clone(),
                        run_id: run_id.clone(),
                    },
                    chat: chat.clone(),
                });

                // Step 2: No webhook configured → done.
                let webhook_info = match hctx.schema_registry.webhook_for(&schema) {
                    Some(w) => w,
                    None => continue,
                };

                // Step 3: Write-ahead — enqueue BEFORE any network attempt.
                let job = DeliveryJob {
                    schema: schema.clone(),
                    value,
                    run_id: run_id.clone(),
                    source_chat_binding: chat.clone(),
                };
                let queue_path = match hctx.delivery_queue.enqueue(&job).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Failed to enqueue delivery job {}: {}", run_id, e);
                        send_error(
                            ctx,
                            &format!("Failed to queue webhook delivery: {}", e),
                            telegram,
                            slack,
                            discord,
                        )
                        .await;
                        continue;
                    }
                };

                // Step 4: Sync attempt.
                // Rename to `.delivering.json` before attempting delivery so the
                // retry worker (which only scans `*.json`) cannot pick up the same
                // file concurrently and cause a duplicate POST.
                let delivering_path = match hctx.delivery_queue.mark_delivering(&queue_path).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to mark job as delivering (will be retried by worker): {}",
                            e
                        );
                        // Leave the file in pending/; the retry worker will deliver it.
                        let pending = hctx.delivery_queue.pending_count().await.unwrap_or(1);
                        send_reply(
                            ctx,
                            &format!("⏳ queued for retry ({} pending)", pending),
                            telegram,
                            slack,
                            discord,
                        )
                        .await;
                        continue;
                    }
                };

                match hctx.webhook_client.deliver(&job, &webhook_info).await {
                    Ok(elapsed) => {
                        // On success: remove the delivering file (no retry needed).
                        let _ = tokio::fs::remove_file(&delivering_path).await;
                        send_reply(
                            ctx,
                            &format!("✅ delivered to webhook ({}ms)", elapsed.as_millis()),
                            telegram,
                            slack,
                            discord,
                        )
                        .await;
                    }
                    Err(_) => {
                        // On failure: rename back to `.json` so retry worker picks it up.
                        if let Err(e) = hctx
                            .delivery_queue
                            .unmark_delivering(&delivering_path)
                            .await
                        {
                            tracing::error!(
                                "Failed to unmark delivering job (manual recovery may be needed): {}",
                                e
                            );
                        }
                        let pending = hctx.delivery_queue.pending_count().await.unwrap_or(1);
                        send_reply(
                            ctx,
                            &format!("⏳ queued for retry ({} pending)", pending),
                            telegram,
                            slack,
                            discord,
                        )
                        .await;
                    }
                }
            }
        }
    }

    (got_any_output, session_id_out)
}

async fn send_reply(
    ctx: &ReplyContext,
    text: &str,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    discord: Option<&dyn ChatPlatform>,
) {
    // Socket-origin: route to the per-request response channel.
    if let Some(ref tx) = ctx.socket_reply_tx {
        let _ = tx.send(text.to_string());
        return;
    }
    use crate::chat_adapters::PlatformType;
    let platform: Option<&dyn ChatPlatform> = match ctx.platform {
        PlatformType::Telegram => telegram,
        PlatformType::Slack => slack,
        PlatformType::Discord => discord,
    };
    if let Some(p) = platform {
        if let Err(e) = p
            .send_message(text, &ctx.chat_id, ctx.thread_ts.as_deref())
            .await
        {
            tracing::error!("Failed to send reply: {}", e);
        }
    }
}

async fn send_error(
    ctx: &ReplyContext,
    error: &str,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    discord: Option<&dyn ChatPlatform>,
) {
    send_reply(ctx, &format!("Error: {}", error), telegram, slack, discord).await;
}

async fn send_photo_reply(
    ctx: &ReplyContext,
    data: &[u8],
    filename: &str,
    caption: Option<&str>,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    discord: Option<&dyn ChatPlatform>,
) {
    // Socket-origin: send a text fallback since the socket channel is text-only.
    if let Some(ref tx) = ctx.socket_reply_tx {
        let _ = tx.send(format!("[file: {}]", filename));
        return;
    }
    let platform: Option<&dyn ChatPlatform> = match ctx.platform {
        PlatformType::Telegram => telegram,
        PlatformType::Slack => slack,
        PlatformType::Discord => discord,
    };
    if let Some(p) = platform {
        if let Err(e) = p
            .send_photo(
                data,
                filename,
                caption,
                &ctx.chat_id,
                ctx.thread_ts.as_deref(),
            )
            .await
        {
            tracing::error!("Failed to send photo: {}", e);
        }
    }
}

async fn send_document_reply(
    ctx: &ReplyContext,
    data: &[u8],
    filename: &str,
    caption: Option<&str>,
    telegram: Option<&dyn ChatPlatform>,
    slack: Option<&dyn ChatPlatform>,
    discord: Option<&dyn ChatPlatform>,
) {
    // Socket-origin: send a text fallback since the socket channel is text-only.
    if let Some(ref tx) = ctx.socket_reply_tx {
        let _ = tx.send(format!("[file: {}]", filename));
        return;
    }
    let platform: Option<&dyn ChatPlatform> = match ctx.platform {
        PlatformType::Telegram => telegram,
        PlatformType::Slack => slack,
        PlatformType::Discord => discord,
    };
    if let Some(p) = platform {
        if let Err(e) = p
            .send_document(
                data,
                filename,
                caption,
                &ctx.chat_id,
                ctx.thread_ts.as_deref(),
            )
            .await
        {
            tracing::error!("Failed to send document: {}", e);
        }
    }
}

/// Split a message into chunks of at most `max_len` characters,
/// breaking at newline boundaries when possible.
pub(crate) fn split_message(text: &str, max_len: usize) -> Vec<String> {
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
        let split_at = remaining[..safe_limit]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(safe_limit);
        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── HarnessKind ──────────────────────────────────────────────────────────

    #[test]
    fn harness_kind_from_str_opencode_returns_opencode() {
        assert_eq!(
            HarnessKind::from_str("opencode"),
            Some(HarnessKind::Opencode)
        );
        assert_eq!(
            HarnessKind::from_str("OpenCode"),
            Some(HarnessKind::Opencode)
        );
        assert_eq!(
            HarnessKind::from_str("OPENCODE"),
            Some(HarnessKind::Opencode)
        );
    }

    #[test]
    fn harness_kind_name_opencode_returns_opencode_pascal() {
        assert_eq!(HarnessKind::Opencode.name(), "OpenCode");
    }

    #[test]
    fn harness_kind_from_str_claude_roundtrip() {
        let kind = HarnessKind::from_str("claude").expect("claude must parse");
        assert_eq!(kind.name(), "Claude");
    }

    #[test]
    fn harness_kind_from_str_unknown_returns_none() {
        assert_eq!(HarnessKind::from_str("copilot"), None);
    }

    // ── build_session_key ────────────────────────────────────────────────────

    #[test]
    fn build_session_key_claude_foo_returns_prefixed() {
        assert_eq!(build_session_key(HarnessKind::Claude, "foo"), "claude:foo");
    }

    #[test]
    fn build_session_key_opencode_foo_returns_prefixed() {
        assert_eq!(
            build_session_key(HarnessKind::Opencode, "foo"),
            "opencode:foo"
        );
    }

    #[test]
    fn build_session_key_gemini_codex_coverage() {
        assert_eq!(build_session_key(HarnessKind::Gemini, "x"), "gemini:x");
        assert_eq!(build_session_key(HarnessKind::Codex, "x"), "codex:x");
    }

    #[test]
    fn build_session_key_no_collision_across_kinds() {
        // Cross-kind no-collision: same short name yields distinct prefixed keys.
        let k_claude = build_session_key(HarnessKind::Claude, "foo");
        let k_opencode = build_session_key(HarnessKind::Opencode, "foo");
        assert_ne!(k_claude, k_opencode);
    }

    // ── format_tool_event ────────────────────────────────────────────────────

    #[test]
    fn format_tool_event_read_contains_icon_tool_and_description() {
        let result = format_tool_event("Read", "file.rs");
        assert!(result.contains("📖"), "expected book icon");
        assert!(result.contains("Read"), "expected tool name");
        assert!(result.contains("file.rs"), "expected description");
    }

    #[test]
    fn format_tool_event_full_with_input_output_prefers_structured() {
        let result = format_tool_event_full("Bash", "ignored", Some("ls -la"), Some("total 42"));
        assert!(result.contains("ls -la"), "expected input preview");
        assert!(result.contains("total 42"), "expected output preview");
        assert!(
            !result.contains("ignored"),
            "description should be ignored when structured fields present"
        );
    }

    #[test]
    fn format_tool_event_full_falls_back_to_description_when_only_input() {
        let result = format_tool_event_full("Bash", "raw description", Some("ls"), None);
        assert!(
            result.contains("raw description"),
            "should fall back to description when output is None"
        );
    }

    #[test]
    fn format_tool_event_thinking_contains_brain_icon_and_tool_name() {
        let result = format_tool_event("Thinking", "");
        assert!(result.contains("🧠"), "expected brain icon");
        assert!(result.contains("Thinking"), "expected tool name");
    }

    #[test]
    fn format_tool_event_write_contains_pencil_icon() {
        let result = format_tool_event("Write", "some/long/path");
        assert!(result.contains("📝"), "expected pencil icon");
    }

    #[test]
    fn format_tool_event_unknown_tool_contains_wrench_icon() {
        let result = format_tool_event("UnknownTool", "arg");
        assert!(
            result.contains("🔧"),
            "expected wrench icon for unknown tool"
        );
    }

    #[test]
    fn format_tool_event_empty_description_has_no_trailing_space() {
        let result = format_tool_event("Bash", "");
        assert!(!result.ends_with(' '), "should not have trailing space");
        assert!(result.contains("💻"), "expected computer icon");
        assert!(result.contains("Bash"), "expected tool name");
    }

    #[test]
    fn format_tool_event_long_description_is_truncated_with_ellipsis() {
        let long_desc = "x".repeat(100);
        let result = format_tool_event("Read", &long_desc);
        assert!(result.contains("..."), "expected truncation ellipsis");
        // The description portion should not exceed 80 chars + "..."
        // Full result is icon + space + tool + space + truncated_desc
        let desc_part = result
            .splitn(3, ' ')
            .nth(2)
            .expect("result should have at least 3 space-separated parts");
        // 80 chars of content + 3 for "..." = 83 chars max for the truncated portion
        assert!(
            desc_part.chars().count() <= 83,
            "truncated description too long: {} chars",
            desc_part.chars().count()
        );
    }

    #[test]
    fn format_tool_event_description_exactly_at_80_chars_is_not_truncated() {
        let desc = "y".repeat(80);
        let result = format_tool_event("Grep", &desc);
        assert!(
            !result.contains("..."),
            "80-char description should not be truncated"
        );
    }

    // ── truncate ─────────────────────────────────────────────────────────────

    #[test]
    fn truncate_short_string_is_returned_unchanged() {
        let s = "hello";
        assert_eq!(truncate(s, 10), "hello");
    }

    #[test]
    fn truncate_string_exactly_at_limit_is_returned_unchanged() {
        let s = "hello";
        assert_eq!(truncate(s, 5), "hello");
    }

    #[test]
    fn truncate_string_over_limit_ends_with_ellipsis() {
        let result = truncate("hello world", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn truncate_unicode_string_does_not_panic() {
        // Each emoji is >1 byte; slicing at a byte boundary would panic without char-aware logic
        let emoji_str = "🎉🎊🎈🎁🎀🎆🎇✨🌟⭐";
        let result = truncate(emoji_str, 3);
        assert!(result.ends_with("..."), "should end with ellipsis");
        // Should not have sliced through a multi-byte char
        assert!(
            std::str::from_utf8(result.as_bytes()).is_ok(),
            "must be valid UTF-8"
        );
    }

    #[test]
    fn truncate_empty_string_returns_empty() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_cjk_unicode_does_not_panic() {
        let cjk = "中文字符测试内容比较长";
        let result = truncate(cjk, 4);
        assert!(result.ends_with("..."), "should end with ellipsis");
        assert!(
            std::str::from_utf8(result.as_bytes()).is_ok(),
            "must be valid UTF-8"
        );
    }

    // ── split_message ────────────────────────────────────────────────────────

    #[test]
    fn split_message_short_text_returns_single_chunk() {
        let chunks = split_message("hello", 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn split_message_text_over_limit_returns_multiple_chunks() {
        let text = "a".repeat(200);
        let chunks = split_message(&text, 100);
        assert!(chunks.len() >= 2, "expected multiple chunks");
    }

    #[test]
    fn split_message_prefers_newline_boundary_over_mid_word_split() {
        // "aaaa\n" is 5 chars; "bbbb" is 4; total 9 > 8 limit.
        // Should split at the newline so first chunk is "aaaa\n".
        let text = "aaaa\nbbbb";
        let chunks = split_message(text, 8);
        assert!(
            chunks[0].ends_with('\n') || chunks[0] == "aaaa\n",
            "first chunk should end at newline boundary, got: {:?}",
            chunks[0]
        );
        assert!(chunks.last().unwrap().contains("bbbb"));
    }

    #[test]
    fn split_message_text_exactly_at_limit_returns_single_chunk() {
        let text = "a".repeat(50);
        let chunks = split_message(&text, 50);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_message_emoji_text_does_not_panic_and_produces_valid_utf8() {
        // emoji are 4 bytes each; 100 of them = 400 bytes but only 100 chars
        let text = "🚀".repeat(100);
        let chunks = split_message(&text, 50);
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        for chunk in &chunks {
            assert!(
                std::str::from_utf8(chunk.as_bytes()).is_ok(),
                "chunk is not valid UTF-8"
            );
        }
    }

    #[test]
    fn split_message_cjk_text_does_not_panic_and_produces_valid_utf8() {
        // CJK characters are 3 bytes each
        let text = "字".repeat(200);
        let chunks = split_message(&text, 50);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        }
    }

    #[test]
    fn split_message_reassembled_chunks_equal_original_text() {
        let text = "line one\nline two\nline three\nline four\nline five";
        let chunks = split_message(text, 15);
        let reassembled = chunks.join("");
        assert_eq!(
            reassembled, text,
            "reassembled chunks should equal original"
        );
    }

    #[test]
    fn split_message_no_chunk_exceeds_max_len_by_more_than_char_boundary_slack() {
        let text = "🎉".repeat(50); // 200 bytes, 50 chars
        let max_len = 30;
        let chunks = split_message(&text, max_len);
        for chunk in &chunks {
            // Allow a small slack for char-boundary rounding (one emoji = 4 bytes)
            assert!(
                chunk.len() <= max_len + 4,
                "chunk byte length {} exceeds max {} by more than one char",
                chunk.len(),
                max_len
            );
        }
    }
}
