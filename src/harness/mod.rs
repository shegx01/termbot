pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::buffer::{StreamEvent, StructuredOutputPayload};
use crate::chat_adapters::{Attachment, ChatBinding, ChatPlatform, PlatformType, ReplyContext};
use crate::command::HarnessOptions;
use crate::delivery::split_message;
use crate::socket::events::AmbientEvent;
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

    /// Lowercase identifier used in user-facing error prefixes (e.g. panic
    /// messages, schema error redirects, log fields) and in cross-harness
    /// state-key construction via [`build_session_key`]. Matches `from_str`'s
    /// expected input so round-tripping is lossless.
    pub(crate) fn cli_label(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
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
    format!("{}:{}", kind.cli_label(), name)
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

/// Blanket impl: every `Arc<H>` is itself a `Harness` when `H: Harness`.
///
/// This replaces three near-identical hand-rolled `impl Harness for
/// Arc<HarnessXxx>` forwarders (one each for codex, gemini, opencode). The
/// motivating use site is `App::new`, which holds a typed `Arc<OpencodeHarness>`
/// for direct shutdown access while also inserting a clone into the
/// type-erased `harnesses: HashMap<HarnessKind, Box<dyn Harness>>` map.
///
/// Bounds rationale:
/// - `H: Harness` — `Arc<H>` only forwards calls the trait already supports.
/// - `Send + Sync` — required because the trait itself is `Send + Sync` and
///   the blanket must satisfy it for the resulting `Arc<H>` to be insertable
///   into `Box<dyn Harness>`.
/// - `'static` — necessary so the boxed `dyn Harness` satisfies `'static` (the
///   trait-object default lifetime), otherwise `Box::new(arc) as Box<dyn
///   Harness>` would fail.
#[async_trait]
impl<H> Harness for std::sync::Arc<H>
where
    H: Harness + Send + Sync + 'static,
{
    fn kind(&self) -> HarnessKind {
        (**self).kind()
    }

    fn supports_resume(&self) -> bool {
        (**self).supports_resume()
    }

    async fn run_prompt(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        cwd: &Path,
        session_id: Option<&str>,
        options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        (**self)
            .run_prompt(prompt, attachments, cwd, session_id, options)
            .await
    }

    fn get_session_id(&self, session_name: &str) -> Option<String> {
        (**self).get_session_id(session_name)
    }

    fn set_session_id(&self, session_name: &str, session_id: String) {
        (**self).set_session_id(session_name, session_id)
    }
}

/// Sanitize a stderr string before forwarding it to chat. Shared base for the
/// codex, gemini, and opencode harnesses; each may pre-filter known-benign
/// log lines via a small per-harness wrapper before calling this.
///
/// The transformation pipeline:
/// 1. **Env-var redaction** — `KEY=value` patterns where `KEY` starts with an
///    ASCII alphabetic char and contains only `[A-Za-z0-9_]` are replaced
///    with `KEY=<redacted>`. Values are line-bounded (`\n` / `\r`) rather
///    than whitespace-bounded so a quoted secret like `KEY='secret with
///    spaces'` is fully redacted. Slight over-redaction of prose like
///    `name=hello` is acceptable; the alternative — missing a real key —
///    leaks credentials into the chat log.
/// 2. **Absolute-user-path redaction** — `/Users/<name>/…` and `/home/<name>/…`
///    prefixes are replaced with `<redacted-path>`.
/// 3. **500-char truncation** — applied last so the redaction can't leak a
///    partial prefix near the boundary.
///
/// The raw content should be logged at `tracing::debug!` by the caller so
/// operators can diagnose via `RUST_LOG=debug` without shipping raw content.
pub(crate) fn sanitize_stderr_base(s: &str) -> String {
    // (1) Env-var redaction.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(pos) = rest.find('=') {
            let before = &rest[..pos];
            let key_start = before
                .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map(|i| i + 1)
                .unwrap_or(0);
            let key = &before[key_start..];
            let is_env_key = !key.is_empty()
                && key.starts_with(|c: char| c.is_ascii_alphabetic())
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

            if is_env_key {
                out.push_str(&rest[..key_start]);
                out.push_str(key);
                out.push('=');
                out.push_str("<redacted>");
                let after_eq = &rest[pos + 1..];
                // Line-bounded (NOT whitespace-bounded) so quoted values
                // containing spaces don't leak.
                let val_end = after_eq.find(['\n', '\r']).unwrap_or(after_eq.len());
                rest = &after_eq[val_end..];
                continue;
            }

            out.push_str(&rest[..pos + 1]);
            rest = &rest[pos + 1..];
        } else {
            out.push_str(rest);
            break;
        }
    }

    // (2) Absolute user-path redaction.
    //
    // Replace the entire path-shaped run starting at `/Users/<u>/…` or
    // `/home/<u>/…` with a single `<redacted-path>` placeholder, ending at
    // the first whitespace or end-of-line. The previous implementation
    // skipped only past the username + first `/`, which let
    // `<redacted-path>secret-project/key.pem` survive — leaking directory
    // structure that's often as sensitive as the username (C6.P1.1).
    //
    // A path component is anything not ASCII-whitespace and not a path
    // terminator like `:` or `,`. We use the conservative rule "advance
    // until whitespace or end-of-line" since most stderr prints paths
    // followed by a space or `\n`. Trailing punctuation like `:42` or `,`
    // after a path is preserved by stopping at non-path characters.
    let patterns: &[&str] = &["/Users/", "/home/"];
    let mut result = out;
    for needle in patterns {
        if result.contains(needle) {
            let mut new_result = String::with_capacity(result.len());
            let mut scan = result.as_str();
            while let Some(idx) = scan.find(needle) {
                new_result.push_str(&scan[..idx]);
                new_result.push_str("<redacted-path>");
                // Advance past the entire path. Stop at whitespace
                // (space / tab / newline) or end-of-string. This redacts
                // both `<u>` and every nested directory + filename so the
                // chat output reveals nothing about the layout.
                let after_needle = &scan[idx..];
                let path_end = after_needle
                    .find(|c: char| c.is_ascii_whitespace())
                    .unwrap_or(after_needle.len());
                scan = &after_needle[path_end..];
            }
            new_result.push_str(scan);
            result = new_result;
        }
    }

    // (3) 500-char truncation.
    result.chars().take(500).collect()
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", &s[..end])
}

/// Format a panic payload as a chat-safe error string for the given harness.
///
/// Used inside each subprocess harness's `tokio::spawn` + `catch_unwind` panic
/// handler so a fault in the spawned task surfaces as a `HarnessEvent::Error`
/// instead of crashing the process. `info` is the dereferenced
/// `Box<dyn Any + Send>` returned by `catch_unwind` on a panic.
#[must_use]
pub(crate) fn format_panic_message(kind: HarnessKind, info: &(dyn std::any::Any + Send)) -> String {
    let label = kind.cli_label();
    if let Some(s) = info.downcast_ref::<&str>() {
        format!("{}: internal panic: {}", label, s)
    } else if let Some(s) = info.downcast_ref::<String>() {
        format!("{}: internal panic: {}", label, s)
    } else {
        format!("{}: internal panic (unknown)", label)
    }
}

/// Strip ANSI escape sequences (CSI sequences and simple OSC). Hand-rolled to
/// avoid adding a crate dependency — matches `\x1b[...m`, `\x1b[...K`, and
/// the OSC titlebar form `\x1b]...(\x07|\x1b\\)`. Moved from the opencode
/// harness; the helper is `pub(crate)` so future subprocess harnesses can call
/// it from the shared lifecycle path.
#[must_use]
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    for cc in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&cc) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    chars.next();
                    while let Some(cc) = chars.next() {
                        if cc == '\x07' {
                            break;
                        }
                        if cc == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Two-char ESC (e.g. `\x1bM` reverse-index, `\x1b=`
                    // DECKPAM): consume both bytes so the trailing char
                    // doesn't leak into the output.
                    chars.next();
                }
                None => {
                    // Lone trailing ESC at end of input — drop and exit.
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Thread-safe `name → session_id` map shared across the subprocess harnesses.
///
/// Wraps an `Arc<StdMutex<HashMap<String, String>>>`. All operations are
/// poison-recovering — if a holder panicked while inside the critical section,
/// readers and writers can still proceed because the data type doesn't
/// observe partial mutations. `Clone` is load-bearing: harnesses clone the
/// store before moving it into a `tokio::spawn` task.
#[derive(Default, Clone)]
pub(crate) struct SessionStore {
    inner: Arc<StdMutex<HashMap<String, String>>>,
}

impl SessionStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up a session id by user-supplied name.
    pub(crate) fn get(&self, name: &str) -> Option<String> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(name).cloned()
    }

    /// Insert (or overwrite) a session id for a user-supplied name.
    pub(crate) fn set(&self, name: &str, id: String) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(name.to_string(), id);
    }

    /// Test-only: borrow the underlying mutex so tests can inject a poison
    /// condition (panic while holding the guard) and verify that `get`/`set`
    /// recover. Compiled only under `#[cfg(test)]`.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &StdMutex<HashMap<String, String>> {
        &self.inner
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Subprocess lifecycle helpers
//
// Shared by the three CLI-subprocess harnesses (opencode/gemini/codex). The
// claude harness uses the `claude-agent-sdk-rust` crate (no subprocess) and
// does not call any of these.
//
// The lifecycle helpers below assume they're called from inside a
// `tokio::spawn(async move { … })` task. None of them spawn the task on the
// caller's behalf; the caller owns the spawn so it can move per-harness state
// into the closure freely.
// ───────────────────────────────────────────────────────────────────────────

/// Configuration for [`run_subcommand`] — the shared runner used by the
/// chat-safe subcommand passthroughs in opencode/gemini/codex.
///
/// Per-harness divergences are flags on this struct so the
/// spawn/timeout/sanitize/fence pipeline lives in one place.
#[derive(Debug)]
pub(crate) struct SubcommandConfig {
    /// Harness kind — used for label prefixes in error messages.
    pub kind: HarnessKind,
    /// Display name of the subcommand (e.g. `"auth list"`, `"session list"`).
    pub sub_label: String,
    /// User-supplied extra args (rendered into the empty-output marker only).
    pub extra_args: Vec<String>,
    /// Strip ANSI color codes from stdout before fencing. Currently set only
    /// by the opencode harness.
    pub strip_ansi: bool,
    /// Truncate stdout to this character count and append a hint line. `None`
    /// = no truncation. Set to `Some(3000)` by codex for chat-friendliness.
    pub max_chars: Option<usize>,
    /// Hint string appended in parentheses to the "binary not found" error.
    pub install_hint: &'static str,
    /// Stderr sanitizer — each harness can pre-filter benign log lines before
    /// the shared base redacts env-vars/paths and truncates.
    /// `sanitize_stderr_base` is the no-prefilter default.
    pub sanitize: fn(&str) -> String,
}

/// Run a chat-safe subcommand of a subprocess CLI: spawn → 30s timeout →
/// sanitize stderr → emit `Text` (or `Error`) followed by `Done`.
///
/// Identical pattern across opencode/gemini/codex; per-harness divergences
/// (ANSI strip, truncation, stderr pre-filter) are flags on
/// [`SubcommandConfig`]. Always emits exactly one `Done` event before
/// returning, regardless of the success/failure path, so callers can rely on
/// the receiver eventually terminating.
pub(crate) async fn run_subcommand(
    binary: PathBuf,
    argv: Vec<String>,
    cfg: SubcommandConfig,
    event_tx: mpsc::Sender<HarnessEvent>,
) {
    let label = cfg.kind.cli_label();

    let mut cmd = Command::new(&binary);
    cmd.args(&argv)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{} binary not found: {} ({})",
                    label,
                    binary.display(),
                    cfg.install_hint
                )
            } else {
                format!("{} {} spawn failed: {}", label, cfg.sub_label, e)
            };
            send_subcommand_failure(&event_tx, msg).await;
            return;
        }
    };

    let out =
        match tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                send_subcommand_failure(
                    &event_tx,
                    format!("{} {} wait failed: {}", label, cfg.sub_label, e),
                )
                .await;
                return;
            }
            Err(_) => {
                send_subcommand_failure(
                    &event_tx,
                    format!("{} {} timed out after 30s", label, cfg.sub_label),
                )
                .await;
                return;
            }
        };

    if !out.status.success() {
        // Cap stderr at 64 KiB before parsing so a crash-looping process
        // can't OOM us. Matches the run-prompt path's stderr drain limit.
        let raw_stderr = if out.stderr.len() > 64 * 1024 {
            &out.stderr[..64 * 1024]
        } else {
            &out.stderr[..]
        };
        let stderr_trim = String::from_utf8_lossy(raw_stderr).trim().to_string();
        let sanitized = (cfg.sanitize)(&stderr_trim);
        // Log the sanitized form so RUST_LOG=debug captures don't leak
        // secrets into shared log aggregators.
        tracing::debug!(
            "{} {} non-zero exit; sanitized stderr: {}",
            label,
            cfg.sub_label,
            sanitized
        );
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let detail = if sanitized.is_empty() {
            format!(
                "{} {} exited with status {} (no stderr)",
                label, cfg.sub_label, code
            )
        } else {
            format!(
                "{} {} exited with status {}: {}",
                label, cfg.sub_label, code, sanitized
            )
        };
        send_subcommand_failure(&event_tx, detail).await;
        return;
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stripped_string = if cfg.strip_ansi {
        strip_ansi(&stdout)
    } else {
        stdout
    };
    let stripped = stripped_string.trim();

    let message = if stripped.is_empty() {
        let arg_summary = if cfg.extra_args.is_empty() {
            String::new()
        } else {
            format!(" {}", cfg.extra_args.join(" "))
        };
        format!("{} {}{}: no results", label, cfg.sub_label, arg_summary)
    } else if let Some(max) = cfg.max_chars {
        if stripped.chars().count() > max {
            let truncated: String = stripped.chars().take(max).collect();
            format!(
                "```\n{}\n…[truncated; run from terminal for full output]\n```",
                truncated
            )
        } else {
            format!("```\n{}\n```", stripped)
        }
    } else {
        format!("```\n{}\n```", stripped)
    };

    let _ = event_tx.send(HarnessEvent::Text(message)).await;
    let _ = event_tx
        .send(HarnessEvent::Done {
            session_id: String::new(),
        })
        .await;
}

async fn send_subcommand_failure(tx: &mpsc::Sender<HarnessEvent>, err: String) {
    let _ = tx.send(HarnessEvent::Error(err)).await;
    let _ = tx
        .send(HarnessEvent::Done {
            session_id: String::new(),
        })
        .await;
}

/// Live child-process state returned by [`spawn_subprocess`].
///
/// Callers drive the read loop on `stdout_lines`, then call [`collect_stderr`]
/// before `child.wait()` so the drain task ends cleanly and the buffered
/// stderr is available for sanitization on a non-zero exit.
#[derive(Debug)]
pub(crate) struct SpawnedSubprocess {
    pub child: tokio::process::Child,
    pub stdout_lines: tokio::io::Lines<BufReader<ChildStdout>>,
    /// Drain-task handle that captures the first 64 KiB of stderr and sinks
    /// the rest. **Must** be passed to [`collect_stderr`] in the reap path —
    /// dropping it without consuming holds the child's stderr pipe open and
    /// leaks the spawned tokio task.
    pub stderr_handle: Option<JoinHandle<Vec<u8>>>,
}

/// Spawn a subprocess CLI for streaming NDJSON output. Pipes stdout/stderr,
/// nulls stdin, and starts a concurrent stderr drain task that captures the
/// first 64 KiB into a buffer and forwards anything past the cap to
/// `tokio::io::sink` so the child can never deadlock on a full stderr pipe.
///
/// The post-cap sink-drain is mandatory: without it, a child writing >64 KiB
/// of stderr while we read stdout would fill the OS pipe buffer (~64 KiB on
/// Linux) and block on its next stderr write, which would in turn stall our
/// stdout read until the per-call idle timeout fires. Earlier code in
/// `gemini.rs` was missing this drain and is fixed by adopting the helper.
///
/// On spawn failure or missing-stdout: emits `Error` + `Done` on `event_tx`
/// and returns `None`. Callers can treat `None` as "lifecycle handled, just
/// return".
#[must_use = "spawn_subprocess returns Some(child) on success; dropping the result \
              without binding it kills the child via kill_on_drop and emits no \
              terminal event to the receiver"]
pub(crate) async fn spawn_subprocess(
    binary: &Path,
    argv: &[String],
    cwd: &Path,
    kind: HarnessKind,
    install_hint: &'static str,
    event_tx: &mpsc::Sender<HarnessEvent>,
) -> Option<SpawnedSubprocess> {
    let label = kind.cli_label();

    let mut cmd = Command::new(binary);
    cmd.args(argv)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{} binary not found: {} ({})",
                    label,
                    binary.display(),
                    install_hint
                )
            } else {
                format!("{} spawn failed: {}", label, e)
            };
            send_subcommand_failure(event_tx, msg).await;
            return None;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            send_subcommand_failure(event_tx, format!("{}: stdout pipe missing", label)).await;
            // Reap the now-killed child so the process-table entry is freed.
            // `kill_on_drop` only sends SIGKILL; it does not call `wait()`.
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
    };

    let stderr = child.stderr.take();
    if stderr.is_none() {
        // `Stdio::piped()` was set, so a `None` here is a Tokio-internal
        // pathology — log loud so a stderr-heavy child doesn't silently risk
        // pipe-deadlock against the consumer that's expecting a captured buf.
        tracing::warn!(
            "{}: stderr pipe missing despite Stdio::piped — deadlock risk on stderr-heavy output",
            label
        );
    }
    let stderr_handle: Option<JoinHandle<Vec<u8>>> = stderr.map(|s| {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf: Vec<u8> = Vec::new();
            let mut s = s;
            // Capture up to 64 KiB for sanitization.
            {
                let mut limited = (&mut s).take(64 * 1024);
                let _ = limited.read_to_end(&mut buf).await;
            }
            // Drain anything past the cap into a sink so the child never
            // blocks on stderr writes.
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut s, &mut sink).await;
            let _ = sink.flush().await;
            buf
        })
    });

    let stdout_lines = BufReader::new(stdout).lines();
    Some(SpawnedSubprocess {
        child,
        stdout_lines,
        stderr_handle,
    })
}

/// Await the stderr drain handle and return the captured buffer (or empty on
/// any failure). Always call this in the reap path — the spawned task must
/// run to completion to avoid a leaked tokio task and a half-open pipe.
#[must_use = "the captured stderr buffer is the only place sanitized error \
              text comes from on a non-zero exit; discard it and the user \
              sees an unhelpful 'exited with status N (no stderr)' message"]
pub(crate) async fn collect_stderr(
    handle: Option<JoinHandle<Vec<u8>>>,
    kind: HarnessKind,
) -> Vec<u8> {
    match handle {
        Some(h) => h.await.unwrap_or_else(|e| {
            tracing::warn!("{} stderr drain task failed: {}", kind.cli_label(), e);
            Vec::new()
        }),
        None => Vec::new(),
    }
}

/// Wrap a subprocess-harness body future with the standard panic guard:
/// `AssertUnwindSafe` + `catch_unwind`, on-panic emit `Error` + `Done`,
/// emit `AmbientEvent::HarnessFinished` with the run status, and remove any
/// per-prompt attachment temp files.
///
/// Identical pattern across the three subprocess harnesses' `run_prompt`
/// closures and `run_subcommand` task spawns. Callers still own the
/// `tokio::spawn`; this helper is the inside-the-spawn body so each harness
/// retains control over what's moved into the closure.
///
/// **Done-injection invariant**: the helper emits `Done` *only* when the body
/// panics — bodies are expected to emit their own terminal `Done` on success.
/// `run_subcommand` and the per-harness `run_*_inner` functions both follow
/// this contract.
///
/// **Ambient emission**: `HarnessFinished` is emitted iff `ambient_tx` is
/// `Some(_)`. The `run_id` field is passed through verbatim and is the empty
/// string for subcommand spawns (which always pass `ambient_tx = None`).
///
/// **Cleanup ordering**: `cleanup_paths` are removed *after* `HarnessFinished`
/// so an ambient subscriber that observes the finished event can still read
/// the temp files during event delivery. This matches opencode's prior
/// behavior; the codex panic-guard previously cleaned up earlier — the
/// reorder is intentional and the new ordering is canonical.
///
/// **`Send + 'static` body bound**: every call site of this helper is
/// already inside a `tokio::spawn(async move { ... })` task, which itself
/// requires `Send + 'static`. Stating the bound explicitly here documents
/// the contract and gives a clearer error message at the helper boundary
/// rather than at the spawn site. Bodies that borrow non-`Send` state
/// (e.g. `&self`) must clone Arc handles before constructing the future.
pub(crate) async fn run_with_panic_guard<F>(
    kind: HarnessKind,
    run_id: String,
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
    event_tx: &mpsc::Sender<HarnessEvent>,
    cleanup_paths: &[PathBuf],
    body: F,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let status = match result {
        Ok(()) => "ok",
        Err(panic_info) => {
            let msg = format_panic_message(kind, &*panic_info);
            let _ = event_tx.send(HarnessEvent::Error(msg)).await;
            // The panic short-circuited the inner body before it could emit
            // its own terminal `Done`. Send one here so the receiver doesn't
            // block forever waiting for a terminal event.
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            "error"
        }
    };

    if let Some(tx) = ambient_tx {
        let _ = tx.send(AmbientEvent::HarnessFinished {
            harness: kind.name().to_string(),
            run_id,
            status: status.to_string(),
        });
    }

    for path in cleanup_paths {
        if let Err(e) = tokio::fs::remove_file(path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    "{} cleanup failed for {}: {}",
                    kind.cli_label(),
                    path.display(),
                    e
                );
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_tool_event_full ───────────────────────────────────────────────

    #[test]
    fn format_tool_event_read_contains_icon_tool_and_description() {
        let result = format_tool_event_full("Read", "file.rs", None, None);
        assert!(result.contains("📖"), "expected book icon");
        assert!(result.contains("Read"), "expected tool name");
        assert!(result.contains("file.rs"), "expected description");
    }

    #[test]
    fn format_tool_event_thinking_contains_brain_icon_and_tool_name() {
        let result = format_tool_event_full("Thinking", "", None, None);
        assert!(result.contains("🧠"), "expected brain icon");
        assert!(result.contains("Thinking"), "expected tool name");
    }

    #[test]
    fn format_tool_event_write_contains_pencil_icon() {
        let result = format_tool_event_full("Write", "some/long/path", None, None);
        assert!(result.contains("📝"), "expected pencil icon");
    }

    #[test]
    fn format_tool_event_unknown_tool_contains_wrench_icon() {
        let result = format_tool_event_full("UnknownTool", "arg", None, None);
        assert!(
            result.contains("🔧"),
            "expected wrench icon for unknown tool"
        );
    }

    #[test]
    fn format_tool_event_empty_description_has_no_trailing_space() {
        let result = format_tool_event_full("Bash", "", None, None);
        assert!(!result.ends_with(' '), "should not have trailing space");
        assert!(result.contains("💻"), "expected computer icon");
        assert!(result.contains("Bash"), "expected tool name");
    }

    #[test]
    fn format_tool_event_long_description_is_truncated_with_ellipsis() {
        let long_desc = "x".repeat(100);
        let result = format_tool_event_full("Read", &long_desc, None, None);
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
        let result = format_tool_event_full("Grep", &desc, None, None);
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

    // ── strip_ansi ──────────────────────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_csi_color_codes() {
        let input = "\x1b[31mred\x1b[0m";
        assert_eq!(strip_ansi(input), "red");
    }

    #[test]
    fn strip_ansi_removes_erase_codes() {
        let input = "\x1b[2Kline";
        assert_eq!(strip_ansi(input), "line");
    }

    #[test]
    fn strip_ansi_passes_through_plain_text() {
        let input = "hello world";
        assert_eq!(strip_ansi(input), "hello world");
    }

    #[test]
    fn strip_ansi_handles_osc_titlebar_sequence() {
        let input = "\x1b]0;title\x07body";
        assert_eq!(strip_ansi(input), "body");
    }

    #[test]
    fn strip_ansi_consumes_two_char_escapes_without_leaking_trailing_byte() {
        // `\x1bM` (RI / Reverse Index) and `\x1b=` (DECKPAM) are two-byte
        // sequences. Both bytes must be consumed; the trailing char must not
        // appear in the output.
        assert_eq!(strip_ansi("a\x1bMb"), "ab");
        assert_eq!(strip_ansi("x\x1b=y"), "xy");
    }

    // ── format_panic_message ────────────────────────────────────────────────

    #[test]
    fn format_panic_on_str_slice() {
        let info: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(
            format_panic_message(HarnessKind::Opencode, &*info),
            "opencode: internal panic: boom"
        );
    }

    #[test]
    fn format_panic_on_owned_string() {
        let info: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        assert_eq!(
            format_panic_message(HarnessKind::Gemini, &*info),
            "gemini: internal panic: kaboom"
        );
    }

    #[test]
    fn format_panic_on_str_slice_for_claude() {
        let info: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(
            format_panic_message(HarnessKind::Claude, &*info),
            "claude: internal panic: boom"
        );
    }

    #[test]
    fn format_panic_on_unknown_payload() {
        let info: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(
            format_panic_message(HarnessKind::Codex, &*info),
            "codex: internal panic (unknown)"
        );
    }

    // ── HarnessKind::cli_label round-trip ───────────────────────────────────

    #[test]
    fn cli_label_round_trips_through_from_str() {
        for kind in [
            HarnessKind::Claude,
            HarnessKind::Gemini,
            HarnessKind::Codex,
            HarnessKind::Opencode,
        ] {
            let label = kind.cli_label();
            assert_eq!(
                HarnessKind::from_str(label),
                Some(kind),
                "from_str must accept the label produced by cli_label"
            );
        }
    }

    #[test]
    fn from_str_accepts_uppercase_input() {
        // `from_str` lowercases its input before matching; the contract is
        // documented and worth pinning so a stricter match doesn't silently
        // regress callers that pass user-supplied case.
        assert_eq!(HarnessKind::from_str("CLAUDE"), Some(HarnessKind::Claude));
        assert_eq!(
            HarnessKind::from_str("OpenCode"),
            Some(HarnessKind::Opencode)
        );
    }

    // ── SessionStore ────────────────────────────────────────────────────────

    #[test]
    fn session_store_get_and_set_round_trip() {
        let s = SessionStore::new();
        s.set("alpha", "ses_abc".into());
        assert_eq!(s.get("alpha"), Some("ses_abc".into()));
        assert_eq!(s.get("missing"), None);
    }

    #[test]
    fn session_store_set_overwrites_existing_value() {
        let s = SessionStore::new();
        s.set("alpha", "ses_old".into());
        s.set("alpha", "ses_new".into());
        assert_eq!(s.get("alpha"), Some("ses_new".into()));
    }

    #[test]
    fn session_store_get_and_set_recover_from_poisoned_lock() {
        let s = SessionStore::new();
        s.set("alpha", "pre".into());

        let s_panic = s.clone();
        let result = std::thread::spawn(move || {
            let _guard = s_panic.raw().lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(result.is_err(), "panic thread should have unwound");

        // Reads and writes still work despite the poisoned lock.
        assert_eq!(s.get("alpha"), Some("pre".into()));
        s.set("beta", "post".into());
        assert_eq!(s.get("beta"), Some("post".into()));
    }

    // ── run_with_panic_guard ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_with_panic_guard_panicking_body_emits_error_and_done() {
        let (tx, mut rx) = mpsc::channel(8);
        let (ambient_tx, mut ambient_rx) = broadcast::channel(8);

        run_with_panic_guard(
            HarnessKind::Codex,
            "run-1".to_string(),
            Some(ambient_tx),
            &tx,
            &[],
            async {
                panic!("kaboom");
            },
        )
        .await;

        let first = rx.try_recv().expect("expected Error event");
        match first {
            HarnessEvent::Error(msg) => {
                assert!(msg.contains("codex"), "label missing: {}", msg);
                assert!(msg.contains("kaboom"), "payload missing: {}", msg);
            }
            other => panic!("expected Error, got {:?}", other),
        }
        let second = rx.try_recv().expect("expected terminal Done");
        assert!(matches!(second, HarnessEvent::Done { .. }));

        let ev = ambient_rx.try_recv().expect("expected HarnessFinished");
        match ev {
            AmbientEvent::HarnessFinished {
                harness,
                run_id,
                status,
            } => {
                assert_eq!(harness, "Codex");
                assert_eq!(run_id, "run-1");
                assert_eq!(status, "error");
            }
            other => panic!("expected HarnessFinished, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_with_panic_guard_clean_body_emits_ok_status_only() {
        let (tx, mut rx) = mpsc::channel(8);
        let (ambient_tx, mut ambient_rx) = broadcast::channel(8);

        run_with_panic_guard(
            HarnessKind::Gemini,
            "run-2".to_string(),
            Some(ambient_tx),
            &tx,
            &[],
            async {
                // Clean body — no panic, no events emitted.
            },
        )
        .await;

        // No injected Error/Done because the body didn't panic.
        assert!(rx.try_recv().is_err(), "no events expected on clean exit");

        let ev = ambient_rx
            .try_recv()
            .expect("expected HarnessFinished even on clean body");
        match ev {
            AmbientEvent::HarnessFinished {
                harness,
                run_id,
                status,
            } => {
                assert_eq!(harness, "Gemini");
                assert_eq!(run_id, "run-2");
                assert_eq!(status, "ok");
            }
            other => panic!("expected HarnessFinished, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_with_panic_guard_skips_ambient_when_tx_is_none() {
        let (tx, _rx) = mpsc::channel(8);
        // No ambient_tx — should not panic and should not block.
        run_with_panic_guard(
            HarnessKind::Opencode,
            String::new(),
            None,
            &tx,
            &[],
            async { /* clean */ },
        )
        .await;
    }

    #[tokio::test]
    async fn run_with_panic_guard_removes_cleanup_paths_after_run() {
        // Touch two temp files; verify they're gone after run_with_panic_guard.
        let dir = std::env::temp_dir();
        let p1 = dir.join(format!("pr-b-cleanup-{}", ulid::Ulid::new()));
        let p2 = dir.join(format!("pr-b-cleanup-{}", ulid::Ulid::new()));
        tokio::fs::write(&p1, b"x").await.unwrap();
        tokio::fs::write(&p2, b"y").await.unwrap();
        assert!(p1.exists() && p2.exists());

        let (tx, _rx) = mpsc::channel(8);
        run_with_panic_guard(
            HarnessKind::Opencode,
            String::new(),
            None,
            &tx,
            &[p1.clone(), p2.clone()],
            async { /* clean */ },
        )
        .await;

        assert!(!p1.exists(), "cleanup path 1 should be removed");
        assert!(!p2.exists(), "cleanup path 2 should be removed");
    }

    // ── stderr drain (post-cap sink) ────────────────────────────────────────

    #[tokio::test]
    async fn stderr_drain_consumes_past_64_kib_cap_without_deadlock() {
        // Simulates the spawn_subprocess stderr drain task end-to-end
        // without spawning a real subprocess. A deduplicated 128 KiB write
        // through `tokio::io::duplex` exceeds the 64 KiB capture cap; the
        // drain task must consume the rest into `tokio::io::sink` and exit
        // before the 2s timeout.
        use tokio::io::AsyncWriteExt;
        let payload = vec![b'x'; 128 * 1024];
        let (mut writer, reader) = tokio::io::duplex(256 * 1024);
        writer.write_all(&payload).await.unwrap();
        drop(writer);

        let handle: JoinHandle<Vec<u8>> = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf: Vec<u8> = Vec::new();
            let mut s = reader;
            {
                let mut limited = (&mut s).take(64 * 1024);
                let _ = limited.read_to_end(&mut buf).await;
            }
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut s, &mut sink).await;
            let _ = sink.flush().await;
            buf
        });

        let buf = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("drain task must not deadlock past the cap")
            .unwrap();
        assert_eq!(buf.len(), 64 * 1024);
    }

    // ── spawn_subprocess + run_subcommand integration ─────────────

    #[tokio::test]
    async fn spawn_subprocess_emits_error_done_on_not_found() {
        let (tx, mut rx) = mpsc::channel(8);
        let bogus =
            std::path::PathBuf::from(format!("/definitely/not/a/binary/{}", ulid::Ulid::new()));
        let result = spawn_subprocess(
            &bogus,
            &[],
            std::path::Path::new("/"),
            HarnessKind::Opencode,
            "test hint",
            &tx,
        )
        .await;
        assert!(result.is_none(), "NotFound path returns None");

        let first = rx.try_recv().expect("expected Error event");
        match first {
            HarnessEvent::Error(msg) => {
                assert!(msg.contains("opencode"), "label missing: {}", msg);
                assert!(msg.contains("not found"), "msg shape: {}", msg);
                assert!(msg.contains("test hint"), "install hint missing: {}", msg);
            }
            other => panic!("expected Error, got {:?}", other),
        }
        let second = rx.try_recv().expect("expected terminal Done");
        assert!(matches!(second, HarnessEvent::Done { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_subcommand_non_zero_exit_emits_error_with_sanitized_stderr() {
        // Uses /bin/sh as a real binary we can drive deterministically.
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = SubcommandConfig {
            kind: HarnessKind::Codex,
            sub_label: "test".to_string(),
            extra_args: vec![],
            strip_ansi: false,
            max_chars: None,
            install_hint: "n/a",
            sanitize: sanitize_stderr_base,
        };
        run_subcommand(
            std::path::PathBuf::from("/bin/sh"),
            vec!["-c".into(), "echo boom 1>&2; exit 7".into()],
            cfg,
            tx,
        )
        .await;

        let first = rx.try_recv().expect("expected Error event");
        match first {
            HarnessEvent::Error(msg) => {
                assert!(msg.contains("codex"), "label missing: {}", msg);
                assert!(msg.contains("test"), "sub_label missing: {}", msg);
                assert!(msg.contains("status 7"), "exit code missing: {}", msg);
                assert!(msg.contains("boom"), "stderr text missing: {}", msg);
            }
            other => panic!("expected Error, got {:?}", other),
        }
        let second = rx.try_recv().expect("expected terminal Done");
        assert!(matches!(second, HarnessEvent::Done { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_subcommand_empty_stdout_emits_no_results_text() {
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = SubcommandConfig {
            kind: HarnessKind::Opencode,
            sub_label: "models".to_string(),
            extra_args: vec!["--filter".into(), "none".into()],
            strip_ansi: false,
            max_chars: None,
            install_hint: "n/a",
            sanitize: sanitize_stderr_base,
        };
        run_subcommand(
            std::path::PathBuf::from("/bin/sh"),
            vec!["-c".into(), "exit 0".into()],
            cfg,
            tx,
        )
        .await;

        let first = rx.try_recv().expect("expected Text event");
        match first {
            HarnessEvent::Text(msg) => {
                assert!(msg.contains("opencode"), "label missing: {}", msg);
                assert!(msg.contains("models"), "sub_label missing: {}", msg);
                assert!(msg.contains("no results"), "marker missing: {}", msg);
                assert!(msg.contains("--filter none"), "extra args missing: {}", msg);
            }
            other => panic!("expected Text, got {:?}", other),
        }
        assert!(matches!(
            rx.try_recv().expect("Done"),
            HarnessEvent::Done { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_subcommand_truncates_stdout_at_max_chars() {
        // codex's 3000-char path: produce a deterministic 4000-char stdout
        // and assert the helper truncates with the marker line.
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = SubcommandConfig {
            kind: HarnessKind::Codex,
            sub_label: "stats".to_string(),
            extra_args: vec![],
            strip_ansi: false,
            max_chars: Some(3000),
            install_hint: "n/a",
            sanitize: sanitize_stderr_base,
        };
        run_subcommand(
            std::path::PathBuf::from("/bin/sh"),
            vec![
                "-c".into(),
                // 4000 'a' characters via printf — beats the 3000 cap.
                "printf 'a%.0s' $(seq 1 4000)".into(),
            ],
            cfg,
            tx,
        )
        .await;

        let first = rx.try_recv().expect("expected Text event");
        match first {
            HarnessEvent::Text(msg) => {
                assert!(msg.starts_with("```\n"), "fenced opener missing: {:?}", msg);
                assert!(msg.ends_with("\n```"), "fenced closer missing: {:?}", msg);
                assert!(
                    msg.contains("…[truncated; run from terminal for full output]"),
                    "truncation marker missing"
                );
                // The truncation invariant: a 3000-char run of 'a' bytes
                // appears in the body, and no 3001-char run leaks through
                // from the original 4000-char input.
                assert!(
                    msg.contains(&"a".repeat(3000)),
                    "expected a 3000-char run of 'a' bytes in truncated body"
                );
                assert!(
                    !msg.contains(&"a".repeat(3001)),
                    "truncation cap exceeded — found run > 3000"
                );
            }
            other => panic!("expected Text, got {:?}", other),
        }
        assert!(matches!(
            rx.try_recv().expect("Done"),
            HarnessEvent::Done { .. }
        ));
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

    // ── sanitize_stderr_base ─────────────────────────────────────────────────

    #[test]
    fn sanitize_stderr_base_redacts_uppercase_env_var() {
        let out = sanitize_stderr_base("API_KEY=sk-abc123 went bad\n");
        assert!(!out.contains("sk-abc123"), "value leaked: {}", out);
        assert!(out.contains("API_KEY"));
    }

    #[test]
    fn sanitize_stderr_base_redacts_lowercase_env_var() {
        // Lowercase keys (e.g. `gemini_api_key`) must also be redacted —
        // codex / gemini both use this convention.
        let out = sanitize_stderr_base("gemini_api_key=AIzaSy-redact-me\n");
        assert!(!out.contains("AIzaSy-redact-me"), "value leaked: {}", out);
        assert!(out.contains("gemini_api_key"));
    }

    #[test]
    fn sanitize_stderr_base_redacts_value_through_to_end_of_line() {
        // A quoted value with embedded whitespace must redact to end-of-line,
        // not stop at the first space.
        let out = sanitize_stderr_base("KEY='secret with spaces' next-token\n");
        assert!(!out.contains("secret with spaces"), "leaked: {}", out);
    }

    #[test]
    fn sanitize_stderr_base_redacts_user_path() {
        let out = sanitize_stderr_base("crash at /Users/alice/code/foo.rs:42");
        assert!(!out.contains("/Users/alice/"));
        assert!(out.contains("<redacted-path>"));
    }

    #[test]
    fn sanitize_stderr_base_redacts_home_path() {
        let out = sanitize_stderr_base("/home/bob/proj exploded");
        assert!(!out.contains("/home/bob/"));
    }

    #[test]
    fn sanitize_stderr_base_truncates_to_500_chars() {
        let long = "x".repeat(800);
        assert!(sanitize_stderr_base(&long).chars().count() <= 500);
    }

    #[test]
    fn sanitize_stderr_base_preserves_safe_content() {
        let s = "model quota exceeded; try again later";
        assert_eq!(sanitize_stderr_base(s), s);
    }

    /// Critic-6 P1: the previous path-redaction algorithm advanced past
    /// `/Users/<u>/` and resumed scanning, so `secret-project/key.pem` would
    /// survive in the chat output. The new algorithm replaces the entire
    /// path run (until whitespace) with a single placeholder.
    #[test]
    fn sanitize_stderr_base_redacts_full_path_not_just_username() {
        let s = "stat failed: /Users/bob/secret-project/key.pem missing";
        let out = sanitize_stderr_base(s);
        assert!(
            !out.contains("secret-project"),
            "directory after username must be redacted, got: {}",
            out
        );
        assert!(
            !out.contains("key.pem"),
            "filename must be redacted, got: {}",
            out
        );
        // Surrounding prose stays.
        assert!(out.contains("stat failed:"), "prose dropped: {}", out);
        assert!(
            out.contains("missing"),
            "post-path token must be preserved (stops at whitespace), got: {}",
            out
        );
    }

    #[test]
    fn sanitize_stderr_base_redacts_home_path_with_nested_dirs() {
        let s = "/home/alice/code/project/src/main.rs:42";
        let out = sanitize_stderr_base(s);
        assert!(!out.contains("alice"));
        assert!(!out.contains("code"));
        assert!(!out.contains("project"));
        assert!(!out.contains("main.rs"));
        // The trailing `:42` is part of the path-token and gets consumed
        // (no whitespace before EOL); acceptable behaviour — if the input
        // had a space (e.g. `path:42 panic`) we'd preserve `panic`.
    }

    #[test]
    fn sanitize_stderr_base_path_redaction_preserves_post_whitespace_diagnostics() {
        // Bracketing whitespace marks the path boundary so prose survives.
        let s = "error at /Users/dave/etc/config and at /home/dave/.bashrc continues";
        let out = sanitize_stderr_base(s);
        assert!(!out.contains("dave"));
        assert!(!out.contains(".bashrc"));
        assert!(out.contains("error at"));
        assert!(out.contains("continues"));
        assert!(out.contains("and at"), "prose between two paths kept");
    }
}
