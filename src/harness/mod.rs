pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
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
    /// Parse a harness name from user input (case-insensitive). Returns
    /// `None` on unknown names; deliberately uses `Option` rather than the
    /// `FromStr` trait because callers treat unknown input as "not a harness"
    /// and fall through to other parse paths rather than surfacing an error.
    #[allow(clippy::should_implement_trait)]
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

/// Build the prefixed key used to index a named harness session in state.
///
/// Format: `{kind-lowercase}:{name}` (e.g. `claude:auth`, `opencode:build`).
/// Using a prefix prevents collisions when the same human-readable name is
/// used across different harnesses (users expect `claude --name foo` and
/// `opencode --name foo` to be distinct conversations).
pub fn build_session_key(kind: HarnessKind, name: &str) -> String {
    format!("{}:{}", kind.name().to_lowercase(), name)
}

/// Events streamed from a harness session to the chat delivery layer.
/// ToolUse is optional — non-Claude harnesses may only emit Text, Done, Error.
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    /// Harness is using a tool (Read, Write, Edit, Bash, etc.)
    ///
    /// `input` / `output` are additive structured fields populated by harnesses
    /// whose stream protocol separates tool call from tool result (e.g. the
    /// opencode CLI-subprocess harness). When `None`, display falls back to the
    /// legacy `description` string used by the Claude SDK harness.
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

/// Coalesces a harness's separate tool-call and tool-result events into a
/// single [`HarnessEvent::ToolUse`] keyed by `tool_id`.
///
/// Used by harnesses whose underlying CLI emits paired events:
/// - **Gemini**: `tool_use` (with `tool_id`) + `tool_result` (with same
///   `tool_id`, `status: success|error`, `output` or `error`).
/// - **Codex**: `item.started` (with `id`) + `item.completed` (with same
///   `id`, `exit_code`, `aggregated_output`) for tool-kind items
///   (`command_execution`, `file_change`, `mcp_tool_call`, etc.).
///
/// Behavior:
/// - `on_use` stores the partial entry (tool name + input parameters).
/// - `on_result` removes the matching entry and returns a fully-populated
///   [`HarnessEvent::ToolUse`] (or `None` if no matching `tool_use` was
///   observed — the read loop logs these at debug level).
/// - `flush_pending` drains any still-open entries at stream close; each is
///   emitted with `output: None` so the user sees the invocation even when
///   the tool didn't produce a result event before the stream ended.
///
/// Bounded at [`Self::CAP`] entries to prevent unbounded memory growth from
/// a pathological stream that emits many orphaned tool-use events. At cap,
/// `on_use` evicts an arbitrary entry before inserting.
pub(crate) struct ToolPairingBuffer {
    pending: HashMap<String, PendingToolUse>,
}

#[derive(Debug, Clone)]
struct PendingToolUse {
    tool: String,
    input: Option<String>,
}

impl ToolPairingBuffer {
    /// Maximum number of unpaired tool-use entries retained at once.
    /// Reached only by a pathological or drifted stream; in normal
    /// operation the count is small (bounded by model concurrency).
    pub(crate) const CAP: usize = 256;

    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn on_use(&mut self, tool_id: String, tool: String, input: Option<String>) {
        if self.pending.len() >= Self::CAP && !self.pending.contains_key(&tool_id) {
            // Evict an arbitrary entry — order isn't meaningful for resource
            // safety and any concrete ordering would need a separate data
            // structure. Log once per evict so the drift is observable.
            if let Some(k) = self.pending.keys().next().cloned() {
                tracing::warn!(
                    evicted_tool_id = %k,
                    cap = Self::CAP,
                    "tool-pairing buffer at cap; evicting oldest entry"
                );
                self.pending.remove(&k);
            }
        }
        self.pending.insert(tool_id, PendingToolUse { tool, input });
    }

    pub(crate) fn on_result(
        &mut self,
        tool_id: &str,
        success: bool,
        output: Option<String>,
        error: Option<String>,
    ) -> Option<HarnessEvent> {
        let p = self.pending.remove(tool_id)?;
        let description = if success {
            p.tool.clone()
        } else {
            let err_msg = error.as_deref().unwrap_or("tool failed");
            format!("{} ({})", p.tool, err_msg)
        };
        let paired_output = if success { output } else { None };
        Some(HarnessEvent::ToolUse {
            tool: p.tool,
            description,
            input: p.input,
            output: paired_output,
        })
    }

    /// Drain all unpaired tool-use entries and emit each as a
    /// [`HarnessEvent::ToolUse`] with `output: None`.
    pub(crate) fn flush_pending(&mut self) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let drained: Vec<(String, PendingToolUse)> = self.pending.drain().collect();
        for (_, p) in drained {
            out.push(HarnessEvent::ToolUse {
                tool: p.tool.clone(),
                description: p.tool,
                input: p.input,
                output: None,
            });
        }
        out
    }
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
///
/// When `input` is `Some`, prefers the structured input over the legacy
/// `description` string. When `output` is also `Some`, a short preview is
/// appended after an arrow. Falls back to `description` when both structured
/// fields are `None` (preserves Claude-SDK harness behavior).
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

    // Prefer structured input/output when the harness provided them.
    let body = match (input, output) {
        (Some(i), Some(o)) if !i.is_empty() && !o.is_empty() => {
            format!("{} → {}", truncate(i, 60), truncate(o, 40))
        }
        (Some(i), _) if !i.is_empty() => truncate(i, 80),
        _ => {
            if description.is_empty() {
                return format!("{} {}", icon, tool);
            }
            truncate(description, 80)
        }
    };

    format!("{} {} {}", icon, tool, body)
}

/// Legacy two-arg formatter. Kept for call sites that don't have structured
/// `input` / `output`. Forwards to `format_tool_event_full`.
#[allow(dead_code)]
pub fn format_tool_event(tool: &str, description: &str) -> String {
    format_tool_event_full(tool, description, None, None)
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

    // ── format_tool_event ────────────────────────────────────────────────────

    #[test]
    fn format_tool_event_read_contains_icon_tool_and_description() {
        let result = format_tool_event("Read", "file.rs");
        assert!(result.contains("📖"), "expected book icon");
        assert!(result.contains("Read"), "expected tool name");
        assert!(result.contains("file.rs"), "expected description");
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

    // ── ToolPairingBuffer ───────────────────────────────────────────────────
    //
    // Relocated from `src/harness/gemini.rs` when the buffer became a shared
    // utility for both gemini and codex harnesses. Tests are buffer-only and
    // exercise no harness-specific behavior.

    #[test]
    fn pairing_buffer_coalesces_use_and_result_by_tool_id() {
        let mut buf = ToolPairingBuffer::new();
        buf.on_use(
            "tu_1".into(),
            "read_file".into(),
            Some(r#"{"path":"/tmp/x"}"#.into()),
        );
        assert_eq!(buf.pending_len(), 1);

        let paired = buf
            .on_result("tu_1", true, Some("contents".into()), None)
            .expect("result should match tool_use");
        assert_eq!(buf.pending_len(), 0);

        match paired {
            HarnessEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => {
                assert_eq!(tool, "read_file");
                assert_eq!(output.as_deref(), Some("contents"));
                assert!(
                    input.as_deref().unwrap_or("").contains("/tmp/x"),
                    "input should preserve the original parameters"
                );
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn pairing_buffer_result_for_unknown_tool_id_returns_none() {
        let mut buf = ToolPairingBuffer::new();
        let paired = buf.on_result("tu_does_not_exist", true, Some("x".into()), None);
        assert!(
            paired.is_none(),
            "result for unknown tool_id must not fabricate a ToolUse"
        );
    }

    #[test]
    fn pairing_buffer_error_result_suppresses_output_and_decorates_description() {
        let mut buf = ToolPairingBuffer::new();
        buf.on_use("tu_err".into(), "write_file".into(), None);
        let paired = buf
            .on_result("tu_err", false, None, Some("no space left".into()))
            .expect("error result still pairs");
        match paired {
            HarnessEvent::ToolUse {
                description,
                output,
                ..
            } => {
                assert!(
                    description.contains("no space left"),
                    "error message should surface in description, got: {}",
                    description
                );
                assert!(output.is_none(), "output should be None on error result");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn pairing_buffer_flush_emits_unpaired_use_with_none_output() {
        let mut buf = ToolPairingBuffer::new();
        buf.on_use(
            "tu_orphan".into(),
            "bash".into(),
            Some(r#"{"cmd":"ls"}"#.into()),
        );
        let flushed = buf.flush_pending();
        assert_eq!(flushed.len(), 1, "one unpaired use should flush");
        match &flushed[0] {
            HarnessEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => {
                assert_eq!(tool, "bash");
                assert!(input.as_deref().unwrap_or("").contains("ls"));
                assert!(output.is_none(), "flushed entry must have output: None");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert_eq!(buf.pending_len(), 0, "pending must be empty after flush");
    }

    #[test]
    fn pairing_buffer_handles_multiple_interleaved_tools() {
        let mut buf = ToolPairingBuffer::new();
        buf.on_use("a".into(), "tool_a".into(), None);
        buf.on_use("b".into(), "tool_b".into(), None);
        // Results arrive out-of-order.
        let rb = buf
            .on_result("b", true, Some("ob".into()), None)
            .expect("b should pair");
        assert!(matches!(rb, HarnessEvent::ToolUse { ref tool, .. } if tool == "tool_b"));
        let ra = buf
            .on_result("a", true, Some("oa".into()), None)
            .expect("a should pair");
        assert!(matches!(ra, HarnessEvent::ToolUse { ref tool, .. } if tool == "tool_a"));
        assert_eq!(buf.pending_len(), 0);
    }

    #[test]
    fn pairing_buffer_caps_pending_entries_evicting_arbitrary_oldest() {
        let mut buf = ToolPairingBuffer::new();
        for i in 0..ToolPairingBuffer::CAP + 5 {
            buf.on_use(format!("t{}", i), "tool".into(), None);
        }
        assert_eq!(
            buf.pending_len(),
            ToolPairingBuffer::CAP,
            "pending must not exceed CAP"
        );
    }
}
