//! Gemini CLI-subprocess harness.
//!
//! Each prompt spawns `gemini <prompt> -o stream-json [...]` as a short-lived
//! child process. Stdout carries newline-delimited JSON events that we
//! translate into [`HarnessEvent`]s. Session resume is handled via `-r <id>`;
//! `-r latest` is the bare `--continue` analog used when the user does not
//! specify a named session.
//!
//! Unlike the Claude SDK harness there is no persistent sidecar and no
//! lifecycle to manage. `kill_on_drop(true)` on the `Command` ensures any
//! in-flight child terminates when terminus exits.
//!
//! ## Divergence from opencode
//!
//! Opencode's `tool_use` event is atomic (call + result in one JSON line).
//! Gemini emits `tool_use` and `tool_result` as **separate events linked by
//! `tool_id`**. The [`ToolPairingBuffer`] holds open `tool_use` events until
//! their matching `tool_result` arrives (or until the stream closes, at which
//! point any unpaired entries are flushed as `HarnessEvent::ToolUse` with
//! `output: None`).
//!
//! ## Event schema (ground truth: gemini-cli `packages/core/src/output/types.ts`)
//!
//! ```json
//! {"type":"init",        "session_id":"ses_…", "model":"…"}
//! {"type":"message",     "role":"assistant", "content":"…", "delta":true}
//! {"type":"tool_use",    "tool_id":"…", "tool_name":"…", "parameters":{…}}
//! {"type":"tool_result", "tool_id":"…", "status":"success|error", "output":"…", "error":"…"}
//! {"type":"error",       "severity":"warning|error", "message":"…"}
//! {"type":"result",      "status":"success|error", "stats":{…}}
//! ```
//!
//! ## Integration tests
//!
//! Live-binary tests are gated `#[ignore]` + `TERMINUS_HAS_GEMINI=1`.
//! Run with `TERMINUS_HAS_GEMINI=1 cargo test -- --ignored`.

use super::{Harness, HarnessEvent, HarnessKind, ToolPairingBuffer};
use crate::chat_adapters::Attachment;
use crate::command::HarnessOptions;
use crate::config::GeminiConfig;
use crate::socket::events::AmbientEvent;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};

/// Gemini CLI-subprocess harness.
pub struct GeminiHarness {
    config: GeminiConfig,
    /// Map of opaque session-name → gemini session id. Keys here are the raw
    /// user-supplied names; the App layer owns the prefixed `gemini:{name}`
    /// form used for cross-harness isolation in state.
    sessions: Arc<StdMutex<HashMap<String, String>>>,
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
}

impl GeminiHarness {
    /// Construct a harness with the provided config and ambient-event sender.
    pub fn new(config: GeminiConfig, ambient_tx: broadcast::Sender<AmbientEvent>) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: Some(ambient_tx),
        }
    }

    /// Test constructor with no ambient-event broadcast.
    #[cfg(test)]
    pub fn new_with_config(config: GeminiConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: None,
        }
    }
}

#[async_trait]
impl Harness for GeminiHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Gemini
    }

    fn supports_resume(&self) -> bool {
        true
    }

    async fn run_prompt(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        cwd: &Path,
        session_id: Option<&str>,
        options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(256);

        // Gemini-cli has no schema-constrained output surface. Fail fast and
        // redirect users to the claude harness, mirroring opencode's guard.
        if options.schema.is_some() {
            let _ = event_tx
                .send(HarnessEvent::Error(
                    "gemini does not support --schema. Try: `: claude --schema=<name> <prompt>`"
                        .into(),
                ))
                .await;
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            return Ok(event_rx);
        }

        // Attachments are not yet supported by this harness. Surface an error
        // rather than silently dropping user-provided files.
        if !attachments.is_empty() {
            let _ = event_tx
                .send(HarnessEvent::Error(
                    "gemini: attachments are not yet supported — send text only".into(),
                ))
                .await;
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            return Ok(event_rx);
        }

        let binary: PathBuf = self
            .config
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("gemini"));

        let args = build_argv(prompt, session_id, options, &self.config);

        let cwd = cwd.to_path_buf();
        let ambient_tx = self.ambient_tx.clone();
        let run_id = ulid::Ulid::new().to_string();

        {
            let harness_started = AmbientEvent::HarnessStarted {
                harness: HarnessKind::Gemini.name().to_string(),
                run_id: run_id.clone(),
                prompt_hash: None,
            };
            if let Some(ref tx) = ambient_tx {
                let _ = tx.send(harness_started);
            }
        }

        let run_id_for_task = run_id.clone();
        tokio::spawn(async move {
            let body = run_gemini_inner(binary, args, cwd, event_tx.clone());
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> =
                AssertUnwindSafe(body).catch_unwind().await;

            let status = match result {
                Ok(()) => "ok",
                Err(panic_info) => {
                    let msg = format_panic_message(&*panic_info);
                    let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                    // Panic short-circuited `run_gemini_inner` before its own
                    // `Done` emission. Send one here so the receiver doesn't
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
                    harness: HarnessKind::Gemini.name().to_string(),
                    run_id: run_id_for_task,
                    status: status.to_string(),
                });
            }
        });

        Ok(event_rx)
    }

    fn get_session_id(&self, session_name: &str) -> Option<String> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_name).cloned()
    }

    fn set_session_id(&self, session_name: &str, session_id: String) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session_name.to_string(), session_id);
    }
}

/// `Arc<GeminiHarness>` forwarder so `App` can hold a strong named handle and
/// insert a clone into the `harnesses: HashMap<HarnessKind, Box<dyn Harness>>`
/// map without double-boxing.
#[async_trait]
impl Harness for Arc<GeminiHarness> {
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

/// Build the argv for `gemini <prompt> -o stream-json [...]`. Pure function —
/// `session_id` is the *resolved* gemini session id (or `None` for a fresh
/// conversation). When `options.continue_last` is set without a resolved id,
/// `-r latest` is used as the bare-continue analog.
fn build_argv(
    prompt: &str,
    session_id: Option<&str>,
    options: &HarnessOptions,
    config: &GeminiConfig,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-o".into(), "stream-json".into()];

    // Resume: prefer an explicit session id; otherwise `-r latest` when the
    // user asked for bare `--continue`.
    if let Some(sid) = session_id {
        args.push("-r".into());
        args.push(sid.to_string());
    } else if options.continue_last {
        args.push("-r".into());
        args.push("latest".into());
    }

    // Model override: per-prompt options take precedence over static config.
    let effective_model = options.model.as_ref().or(config.model.as_ref());
    if let Some(m) = effective_model {
        args.push("-m".into());
        args.push(m.clone());
    }

    // Approval mode: per-prompt options take precedence over static config.
    let effective_approval = options
        .approval_mode
        .as_ref()
        .or(config.approval_mode.as_ref());
    if let Some(a) = effective_approval {
        args.push("--approval-mode".into());
        args.push(a.clone());
    }

    // Prompt as the final positional (gemini-cli `-p` is deprecated upstream).
    if !prompt.is_empty() {
        args.push(prompt.to_string());
    }

    args
}

/// Sanitize a stderr string before forwarding it to chat. Kept pattern-for-
/// pattern identical to opencode's implementation, with one deliberate
/// divergence: redaction runs **before** the 500-char truncation so a
/// sensitive path near the truncation boundary cannot leak a partial prefix.
///
/// - Redacts `KEY=value` env-var assignments
/// - Redacts `/Users/<name>/...` and `/home/<name>/...` paths
/// - Truncates the redacted result to 500 chars (chat-friendly)
pub(crate) fn sanitize_stderr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(pos) = rest.find('=') {
            let before = &rest[..pos];
            // Include both upper and lower alpha in the key scan so lowercase
            // env-var conventions (e.g. `gemini_api_key=...`) are caught too.
            // False-positives on prose like `name=hello` are acceptable —
            // redacting "hello" is visible-only, whereas missing a key leaks
            // the user's own credentials to the chat log.
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
                let val_end = after_eq
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(after_eq.len());
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

    let patterns: &[(&str, &str)] = &[("/Users/", "/Users/"), ("/home/", "/home/")];
    let mut result = out;
    for (needle, prefix) in patterns {
        if result.contains(needle) {
            let mut new_result = String::with_capacity(result.len());
            let mut scan = result.as_str();
            while let Some(idx) = scan.find(needle) {
                new_result.push_str(&scan[..idx]);
                new_result.push_str("<redacted-path>");
                let after_prefix = &scan[idx + prefix.len()..];
                let skip = after_prefix
                    .find('/')
                    .map(|i| i + 1)
                    .unwrap_or(after_prefix.len());
                scan = &after_prefix[skip..];
            }
            new_result.push_str(scan);
            result = new_result;
        }
    }

    result.chars().take(500).collect()
}

fn format_panic_message(info: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = info.downcast_ref::<&str>() {
        format!("gemini: internal panic: {}", s)
    } else if let Some(s) = info.downcast_ref::<String>() {
        format!("gemini: internal panic: {}", s)
    } else {
        "gemini: internal panic (unknown)".to_string()
    }
}

/// Spawn the gemini child, read NDJSON events, translate them, and drive a
/// [`ToolPairingBuffer`] so emitted `HarnessEvent::ToolUse` entries carry
/// both `input` (from `tool_use`) and `output` (from the matching
/// `tool_result`). Emits `HarnessEvent::Done` on clean terminal event or
/// `HarnessEvent::Error` on protocol/exit failures.
async fn run_gemini_inner(
    binary: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    event_tx: mpsc::Sender<HarnessEvent>,
) {
    let mut cmd = Command::new(&binary);
    cmd.args(&args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "gemini binary not found: {} (set [harness.gemini] binary_path or install gemini on PATH)",
                    binary.display()
                )
            } else {
                format!("gemini spawn failed: {}", e)
            };
            let _ = event_tx.send(HarnessEvent::Error(msg)).await;
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = event_tx
                .send(HarnessEvent::Error(
                    "gemini: stdout pipe missing".to_string(),
                ))
                .await;
            let _ = child.kill().await;
            // Reap the (now-killed) child so the process-table entry is freed.
            // `kill_on_drop` only sends SIGKILL — it does not call `wait()`.
            let _ = child.wait().await;
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            return;
        }
    };

    // Drain stderr concurrently with stdout so gemini-cli doesn't pipe-deadlock
    // when its stderr buffer fills (~64 KiB on Linux, ~8 KiB on macOS).
    // Without a concurrent drain, if gemini writes substantial stderr while
    // also writing stdout, gemini will block on the stderr write — and we'll
    // block on stdout's `next_line()` waiting for output that never comes.
    let stderr_handle: Option<tokio::task::JoinHandle<Vec<u8>>> = child.stderr.take().map(|s| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf: Vec<u8> = Vec::new();
            let mut limited = s.take(64 * 1024);
            let _ = limited.read_to_end(&mut buf).await;
            buf
        })
    });

    let mut reader = BufReader::new(stdout).lines();
    let mut captured_session_id: Option<String> = None;
    let mut saw_recognized = false;
    let mut saw_unknown = false;
    let mut saw_schema_mismatch = false;
    let mut first_unknown_logged = false;
    let mut fatal_error: Option<String> = None;
    let mut pairing = ToolPairingBuffer::new();
    // Per-turn assistant-text accumulator. Grows on streaming `message` events
    // (where `delta: true`); flushed into a single `HarnessEvent::Text` on an
    // atomic message (non-delta), on the next non-message event, at stream
    // close, or when the buffer crosses `ASSISTANT_BUFFER_CAP` (resource-
    // safety auto-flush against a pathological stream that never terminates
    // a turn).
    const ASSISTANT_BUFFER_CAP: usize = 4 * 1024 * 1024;
    let mut assistant_buffer: String = String::new();

    loop {
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(300), reader.next_line()).await;

        let line = match next {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!("gemini: stdout read: {}", e)))
                    .await;
                break;
            }
            Err(_) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(
                        "gemini: no output for 5 minutes — killing subprocess".to_string(),
                    ))
                    .await;
                let _ = child.kill().await;
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("gemini: failed to parse JSON line: {} ({})", e, trimmed);
                continue;
            }
        };

        match translate_event(&value) {
            Some(events) => {
                let mut terminate = false;
                for ev in events {
                    match ev {
                        TranslatedEvent::Init { session_id, .. } => {
                            saw_recognized = true;
                            if !session_id.is_empty() {
                                captured_session_id = Some(session_id);
                            }
                        }
                        TranslatedEvent::Text(t) => {
                            saw_recognized = true;
                            if !t.is_empty() {
                                // Auto-flush before growing past the cap so
                                // the buffer can't grow unboundedly from a
                                // stream that never emits AssistantDone.
                                if assistant_buffer.len() + t.len() > ASSISTANT_BUFFER_CAP
                                    && !assistant_buffer.is_empty()
                                {
                                    let text = std::mem::take(&mut assistant_buffer);
                                    let _ = event_tx.send(HarnessEvent::Text(text)).await;
                                }
                                assistant_buffer.push_str(&t);
                            }
                        }
                        TranslatedEvent::AssistantDone => {
                            saw_recognized = true;
                            if !assistant_buffer.is_empty() {
                                let text = std::mem::take(&mut assistant_buffer);
                                let _ = event_tx.send(HarnessEvent::Text(text)).await;
                            }
                        }
                        TranslatedEvent::ToolUseStart {
                            tool_id,
                            tool,
                            input,
                        } => {
                            saw_recognized = true;
                            // Flush any buffered assistant text before the tool line
                            // so rendering preserves order.
                            if !assistant_buffer.is_empty() {
                                let text = std::mem::take(&mut assistant_buffer);
                                let _ = event_tx.send(HarnessEvent::Text(text)).await;
                            }
                            pairing.on_use(tool_id, tool, input);
                        }
                        TranslatedEvent::ToolResult {
                            tool_id,
                            success,
                            output,
                            error,
                        } => {
                            saw_recognized = true;
                            if let Some(paired) =
                                pairing.on_result(&tool_id, success, output, error)
                            {
                                let _ = event_tx.send(paired).await;
                            } else {
                                tracing::debug!(
                                    "gemini: tool_result for unknown tool_id '{}' — ignoring",
                                    tool_id
                                );
                            }
                        }
                        TranslatedEvent::Warning(msg) => {
                            saw_recognized = true;
                            tracing::warn!("gemini warning: {}", msg);
                        }
                        TranslatedEvent::ErrorFatal(msg) => {
                            saw_recognized = true;
                            fatal_error = Some(msg);
                            terminate = true;
                        }
                        TranslatedEvent::Result {
                            status,
                            error_message,
                        } => {
                            saw_recognized = true;
                            if status != "success" {
                                let detail = error_message
                                    .unwrap_or_else(|| format!("gemini result status: {}", status));
                                fatal_error = Some(detail);
                            }
                            terminate = true;
                        }
                        TranslatedEvent::SchemaMismatch {
                            event_type,
                            missing_field,
                        } => {
                            saw_schema_mismatch = true;
                            tracing::warn!(
                                event_type = %event_type,
                                missing_field = %missing_field,
                                "gemini event schema mismatch — update terminus"
                            );
                        }
                    }
                }
                if terminate {
                    break;
                }
            }
            None => {
                let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                saw_unknown = true;
                if !first_unknown_logged {
                    tracing::warn!(
                        event_type = ty,
                        "gemini emitted an unrecognized event type; version drift possible"
                    );
                    first_unknown_logged = true;
                } else {
                    tracing::debug!("gemini: unknown event type `{}`", ty);
                }
            }
        }
    }

    // Flush any buffered assistant text left over (stream ended without a
    // terminal AssistantDone marker).
    if !assistant_buffer.is_empty() {
        let _ = event_tx
            .send(HarnessEvent::Text(std::mem::take(&mut assistant_buffer)))
            .await;
    }

    // Flush any unpaired tool_use entries: emit them with `output: None` so
    // the user still sees that the tool was invoked.
    for ev in pairing.flush_pending() {
        let _ = event_tx.send(ev).await;
    }

    // Reap the child.
    let status = match child.wait().await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("gemini: child.wait() failed: {}", e);
            None
        }
    };

    // Surface a fatal protocol-level error first if one arrived. `take()` so
    // the guards below can check `fatal_error.is_none()` to avoid duplicate
    // error reports from the stderr-non-zero-exit path and the version-drift
    // fallback.
    let had_fatal = fatal_error.is_some();
    if let Some(msg) = fatal_error.take() {
        let _ = event_tx.send(HarnessEvent::Error(msg)).await;
    }

    // Always await the stderr-drain handle (even on success paths) so the
    // spawned task ends cleanly. Buffered stderr is only used on non-zero-
    // exit paths but the drain task must run to completion to avoid a
    // leaked tokio task and a half-open pipe.
    let raw_stderr_buf: Vec<u8> = match stderr_handle {
        Some(h) => h.await.unwrap_or_default(),
        None => Vec::new(),
    };

    // Non-zero exit → surface sanitized stderr.
    if let Some(status) = status {
        if !status.success() && !had_fatal {
            let stderr_raw = String::from_utf8_lossy(&raw_stderr_buf).trim().to_string();
            let sanitized = sanitize_stderr(&stderr_raw);
            // Log the sanitized form — never the raw bytes — so debug-level
            // logs can't leak secrets when `RUST_LOG=debug` is enabled.
            tracing::debug!("gemini run non-zero exit; stderr: {}", sanitized);
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let detail = if sanitized.is_empty() {
                format!("gemini exited with status {} (no stderr)", code)
            } else {
                format!("gemini exited with status {}: {}", code, sanitized)
            };
            let _ = event_tx.send(HarnessEvent::Error(detail)).await;
        }
    }

    // Surface no-content for version drift scenarios.
    if !saw_recognized {
        let msg = if saw_schema_mismatch {
            "gemini: event schema mismatch (version drift — check `gemini --version`)".to_string()
        } else if saw_unknown {
            "gemini: no recognized events received (version drift — check `gemini --version`)"
                .to_string()
        } else if !had_fatal {
            "gemini: no response content".to_string()
        } else {
            // A fatal error already surfaced; avoid duplicating.
            String::new()
        };
        if !msg.is_empty() {
            let _ = event_tx.send(HarnessEvent::Text(msg)).await;
        }
    }

    let sid = captured_session_id.clone().unwrap_or_default();
    let _ = event_tx.send(HarnessEvent::Done { session_id: sid }).await;
}

/// Pure translation of a single gemini JSON event into zero-or-more
/// [`TranslatedEvent`]s. Returns `None` for unrecognized event types so
/// callers can count them for diagnostics.
///
/// For `message` events, `content` always carries the text and `delta: true`
/// marks it as a streaming chunk. Delta chunks translate to a bare `Text`
/// variant (no `AssistantDone`), so the read loop keeps accumulating until
/// an atomic message, a non-message event, or stream close flushes the
/// buffer.
pub(crate) fn translate_event(event: &serde_json::Value) -> Option<Vec<TranslatedEvent>> {
    let ty = event.get("type").and_then(|t| t.as_str())?;
    match ty {
        "init" => {
            let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) else {
                return Some(vec![TranslatedEvent::SchemaMismatch {
                    event_type: "init".into(),
                    missing_field: "session_id".into(),
                }]);
            };
            let model = event
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(vec![TranslatedEvent::Init {
                session_id: sid.to_string(),
                model,
            }])
        }
        "message" => {
            let role = event.get("role").and_then(|v| v.as_str()).unwrap_or("");
            // User echoes are ignored — they'd just duplicate the user's own
            // input back into chat.
            if role != "assistant" {
                return Some(Vec::new());
            }
            // `content` is required by the upstream MessageEvent interface;
            // `delta` is an optional boolean that marks a streaming chunk.
            let Some(content) = event.get("content").and_then(|v| v.as_str()) else {
                return Some(vec![TranslatedEvent::SchemaMismatch {
                    event_type: "message".into(),
                    missing_field: "content".into(),
                }]);
            };
            let is_delta = event
                .get("delta")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if content.is_empty() {
                if is_delta {
                    Some(Vec::new())
                } else {
                    // Empty atomic message: flush any accumulated buffer.
                    Some(vec![TranslatedEvent::AssistantDone])
                }
            } else if is_delta {
                // Streaming chunk — accumulate; the read loop flushes on the
                // next non-message event or on stream close.
                Some(vec![TranslatedEvent::Text(content.to_string())])
            } else {
                // Atomic, one-shot assistant message.
                Some(vec![
                    TranslatedEvent::Text(content.to_string()),
                    TranslatedEvent::AssistantDone,
                ])
            }
        }
        "tool_use" => {
            let Some(tool_id) = event.get("tool_id").and_then(|v| v.as_str()) else {
                return Some(vec![TranslatedEvent::SchemaMismatch {
                    event_type: "tool_use".into(),
                    missing_field: "tool_id".into(),
                }]);
            };
            let Some(tool_name) = event.get("tool_name").and_then(|v| v.as_str()) else {
                return Some(vec![TranslatedEvent::SchemaMismatch {
                    event_type: "tool_use".into(),
                    missing_field: "tool_name".into(),
                }]);
            };
            let input_json = event
                .get("parameters")
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            Some(vec![TranslatedEvent::ToolUseStart {
                tool_id: tool_id.to_string(),
                tool: tool_name.to_string(),
                input: input_json,
            }])
        }
        "tool_result" => {
            let Some(tool_id) = event.get("tool_id").and_then(|v| v.as_str()) else {
                return Some(vec![TranslatedEvent::SchemaMismatch {
                    event_type: "tool_result".into(),
                    missing_field: "tool_id".into(),
                }]);
            };
            let status = event
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("success");
            let success = status == "success";
            // `output` may be a string or a JSON value; stringify either way.
            let output = event.get("output").map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(v).unwrap_or_default(),
            });
            // Upstream `tool_result.error` is `{type, message}` — extract the
            // message; tolerate a plain string for forward-compat with future
            // schema changes.
            let error = event.get("error").and_then(|e| {
                e.get("message")
                    .and_then(|v| v.as_str())
                    .or_else(|| e.as_str())
                    .map(|s| s.to_string())
            });
            Some(vec![TranslatedEvent::ToolResult {
                tool_id: tool_id.to_string(),
                success,
                output,
                error,
            }])
        }
        "error" => {
            let severity = event
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            let message = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match severity {
                "error" => Some(vec![TranslatedEvent::ErrorFatal(message)]),
                // `"warning"` is the only other value in the upstream type, but
                // any other severity ("info", future additions) is treated as
                // non-fatal to keep the read loop resilient to new levels.
                _ => Some(vec![TranslatedEvent::Warning(message)]),
            }
        }
        "result" => {
            let status = event
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("success")
                .to_string();
            let error_message = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(vec![TranslatedEvent::Result {
                status,
                error_message,
            }])
        }
        _ => None,
    }
}

/// Pure-data intermediate representation emitted by [`translate_event`]. The
/// read loop converts these into `HarnessEvent`s, coalescing tool_use +
/// tool_result pairs via the [`ToolPairingBuffer`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TranslatedEvent {
    /// First-seen `init` event: carries the session id used for `Done`.
    Init {
        session_id: String,
        #[allow(dead_code)]
        model: Option<String>,
    },
    /// A chunk of assistant text (from a message delta or content field).
    Text(String),
    /// Signals the end of an assistant message turn — the read loop flushes
    /// its accumulated delta buffer into a single `HarnessEvent::Text`.
    AssistantDone,
    /// The start of a tool invocation. Held in [`ToolPairingBuffer`] until
    /// the matching `tool_result` arrives.
    ToolUseStart {
        tool_id: String,
        tool: String,
        input: Option<String>,
    },
    /// The result of a previously-started tool invocation.
    ToolResult {
        tool_id: String,
        success: bool,
        output: Option<String>,
        error: Option<String>,
    },
    /// `error` with `severity == "warning"` — logged, does not terminate.
    Warning(String),
    /// `error` with `severity == "error"` — terminates the stream.
    ErrorFatal(String),
    /// Terminal `result` event — ends the read loop after processing.
    ///
    /// `error_message` is the nested `result.error.message` payload, populated
    /// only when `status != "success"`. The read loop surfaces it as the
    /// fatal-error message when present.
    Result {
        status: String,
        error_message: Option<String>,
    },
    /// Recognized event type with a required field absent — indicates schema
    /// drift between terminus and upstream gemini-cli.
    SchemaMismatch {
        event_type: String,
        missing_field: String,
    },
}

// `ToolPairingBuffer` was relocated to `crate::harness` (mod.rs) so the codex
// harness can share the same paired-event coalescing logic. The struct,
// helper, impl, and tests now live in `src/harness/mod.rs`. Gemini imports
// the relocated type below.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::build_session_key;

    fn empty_harness() -> GeminiHarness {
        GeminiHarness::new_with_config(GeminiConfig::default())
    }

    // ── translate_event: init ────────────────────────────────────────────────

    #[test]
    fn translate_init_captures_session_id_and_model() {
        let ev = serde_json::json!({
            "type": "init",
            "session_id": "ses_abc",
            "model": "flash-lite"
        });
        let out = translate_event(&ev).expect("init should translate");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::Init { session_id, model } => {
                assert_eq!(session_id, "ses_abc");
                assert_eq!(model.as_deref(), Some("flash-lite"));
            }
            other => panic!("expected Init, got {:?}", other),
        }
    }

    #[test]
    fn translate_init_missing_session_id_returns_schema_mismatch() {
        let ev = serde_json::json!({"type": "init", "model": "flash"});
        let out = translate_event(&ev).expect("should return SchemaMismatch");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::SchemaMismatch {
                event_type,
                missing_field,
            } => {
                assert_eq!(event_type, "init");
                assert!(missing_field.contains("session_id"));
            }
            other => panic!("expected SchemaMismatch, got {:?}", other),
        }
    }

    // ── translate_event: message (assistant delta / content) ────────────────

    #[test]
    fn translate_message_assistant_delta_chunk_returns_text_without_done() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "hello",
            "delta": true
        });
        let out = translate_event(&ev).expect("delta chunk translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::Text("hello".into()));
    }

    #[test]
    fn translate_message_assistant_content_emits_text_and_assistant_done() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "full answer"
        });
        let out = translate_event(&ev).expect("atomic message translates");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], TranslatedEvent::Text("full answer".into()));
        assert_eq!(out[1], TranslatedEvent::AssistantDone);
    }

    #[test]
    fn translate_message_assistant_delta_false_is_atomic() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "full answer",
            "delta": false
        });
        let out = translate_event(&ev).expect("atomic message with explicit delta=false");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], TranslatedEvent::Text("full answer".into()));
        assert_eq!(out[1], TranslatedEvent::AssistantDone);
    }

    #[test]
    fn translate_message_user_role_is_ignored() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": "the user's prompt"
        });
        let out = translate_event(&ev).expect("user messages map to empty vec");
        assert!(out.is_empty(), "user echoes should not emit events");
    }

    #[test]
    fn translate_message_empty_delta_chunk_returns_empty_vec() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": "",
            "delta": true
        });
        let out = translate_event(&ev).expect("empty delta chunk translates");
        assert!(out.is_empty());
    }

    #[test]
    fn translate_message_empty_atomic_content_emits_assistant_done() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": ""
        });
        let out = translate_event(&ev).expect("empty atomic translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::AssistantDone);
    }

    #[test]
    fn translate_message_missing_content_returns_schema_mismatch() {
        let ev = serde_json::json!({
            "type": "message",
            "role": "assistant"
        });
        let out = translate_event(&ev).expect("should return SchemaMismatch");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::SchemaMismatch {
                event_type,
                missing_field,
            } => {
                assert_eq!(event_type, "message");
                assert_eq!(missing_field, "content");
            }
            other => panic!("expected SchemaMismatch, got {:?}", other),
        }
    }

    // ── translate_event: tool_use / tool_result ─────────────────────────────

    #[test]
    fn translate_tool_use_extracts_tool_id_name_and_input() {
        let ev = serde_json::json!({
            "type": "tool_use",
            "tool_id": "tu_1",
            "tool_name": "read_file",
            "parameters": {"path": "/tmp/foo.txt"}
        });
        let out = translate_event(&ev).expect("tool_use translates");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolUseStart {
                tool_id,
                tool,
                input,
            } => {
                assert_eq!(tool_id, "tu_1");
                assert_eq!(tool, "read_file");
                let input_str = input.as_deref().expect("input should be Some");
                assert!(
                    input_str.contains("/tmp/foo.txt"),
                    "input should contain path, got: {}",
                    input_str
                );
            }
            other => panic!("expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn translate_tool_use_missing_tool_id_returns_schema_mismatch() {
        let ev = serde_json::json!({
            "type": "tool_use",
            "tool_name": "read_file"
        });
        let out = translate_event(&ev).expect("should return SchemaMismatch");
        assert!(matches!(
            out.first(),
            Some(TranslatedEvent::SchemaMismatch { ref missing_field, .. }) if missing_field.contains("tool_id")
        ));
    }

    #[test]
    fn translate_tool_result_success_extracts_output() {
        let ev = serde_json::json!({
            "type": "tool_result",
            "tool_id": "tu_1",
            "status": "success",
            "output": "file contents here"
        });
        let out = translate_event(&ev).expect("tool_result translates");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolResult {
                tool_id,
                success,
                output,
                error,
            } => {
                assert_eq!(tool_id, "tu_1");
                assert!(*success);
                assert_eq!(output.as_deref(), Some("file contents here"));
                assert!(error.is_none());
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_tool_result_error_object_extracts_message() {
        let ev = serde_json::json!({
            "type": "tool_result",
            "tool_id": "tu_2",
            "status": "error",
            "error": {"type": "permission", "message": "permission denied"}
        });
        let out = translate_event(&ev).expect("error tool_result translates");
        match &out[0] {
            TranslatedEvent::ToolResult {
                success,
                error,
                output,
                ..
            } => {
                assert!(!*success);
                assert_eq!(error.as_deref(), Some("permission denied"));
                assert!(output.is_none());
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_tool_result_error_string_fallback() {
        // Forward-compat: a plain-string error (pre-schema-rev or variant)
        // should still be extracted rather than dropped.
        let ev = serde_json::json!({
            "type": "tool_result",
            "tool_id": "tu_2",
            "status": "error",
            "error": "permission denied"
        });
        let out = translate_event(&ev).expect("error tool_result translates");
        match &out[0] {
            TranslatedEvent::ToolResult { error, .. } => {
                assert_eq!(error.as_deref(), Some("permission denied"));
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    // ── translate_event: error severities ───────────────────────────────────

    #[test]
    fn translate_error_severity_error_is_fatal() {
        let ev = serde_json::json!({
            "type": "error",
            "severity": "error",
            "message": "model overloaded"
        });
        let out = translate_event(&ev).expect("error event translates");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            TranslatedEvent::ErrorFatal("model overloaded".into())
        );
    }

    #[test]
    fn translate_error_severity_warning_does_not_terminate() {
        let ev = serde_json::json!({
            "type": "error",
            "severity": "warning",
            "message": "deprecated model"
        });
        let out = translate_event(&ev).expect("warning translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::Warning("deprecated model".into()));
    }

    // ── translate_event: result ─────────────────────────────────────────────

    #[test]
    fn translate_result_success_triggers_terminal_marker() {
        let ev = serde_json::json!({
            "type": "result",
            "status": "success",
            "stats": {"tokens": 42}
        });
        let out = translate_event(&ev).expect("result translates");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::Result {
                status,
                error_message,
            } => {
                assert_eq!(status, "success");
                assert!(error_message.is_none());
            }
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn translate_result_error_captures_error_message() {
        let ev = serde_json::json!({
            "type": "result",
            "status": "error",
            "error": {"type": "quota", "message": "monthly quota exceeded"}
        });
        let out = translate_event(&ev).expect("result translates");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::Result {
                status,
                error_message,
            } => {
                assert_eq!(status, "error");
                assert_eq!(error_message.as_deref(), Some("monthly quota exceeded"));
            }
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn translate_error_unknown_severity_defaults_to_warning() {
        let ev = serde_json::json!({
            "type": "error",
            "severity": "info",
            "message": "heads up"
        });
        let out = translate_event(&ev).expect("error translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::Warning("heads up".into()));
    }

    #[test]
    fn translate_unknown_type_returns_none() {
        let ev = serde_json::json!({"type": "mystery_event"});
        assert!(translate_event(&ev).is_none());
    }

    #[test]
    fn translate_missing_type_returns_none() {
        let ev = serde_json::json!({"session_id": "x"});
        assert!(translate_event(&ev).is_none());
    }

    // ToolPairingBuffer tests live in `src/harness/mod.rs` (relocated when the
    // buffer became a shared utility for both gemini and codex harnesses).

    // ── sanitize_stderr parity with opencode ────────────────────────────────

    #[test]
    fn sanitize_stderr_truncates_at_500_chars() {
        let long = "x".repeat(600);
        let out = sanitize_stderr(&long);
        assert!(out.len() <= 500);
    }

    #[test]
    fn sanitize_stderr_redacts_lowercase_env_var_assignments() {
        let input = "gemini_api_key=AIzaSy-redact-me failed to connect";
        let out = sanitize_stderr(input);
        assert!(
            !out.contains("AIzaSy-redact-me"),
            "lowercase env-var value leaked: {}",
            out
        );
        assert!(
            out.contains("gemini_api_key"),
            "lowercase key name should remain"
        );
    }

    #[test]
    fn sanitize_stderr_redacts_env_var_assignments() {
        let input = "GEMINI_API_KEY=AIzaSy-redact-me failed to connect";
        let out = sanitize_stderr(input);
        assert!(
            !out.contains("AIzaSy-redact-me"),
            "API key value leaked: {}",
            out
        );
        assert!(out.contains("GEMINI_API_KEY"), "key name should remain");
    }

    #[test]
    fn sanitize_stderr_redacts_user_paths() {
        let input = "failed at /Users/alice/code error";
        let out = sanitize_stderr(input);
        assert!(!out.contains("/Users/alice/"), "user path leaked: {}", out);
    }

    #[test]
    fn sanitize_stderr_redacts_home_paths() {
        let input = "/home/bob/thing exploded";
        let out = sanitize_stderr(input);
        assert!(!out.contains("/home/bob/"), "home path leaked: {}", out);
    }

    #[test]
    fn sanitize_stderr_preserves_non_sensitive_content() {
        let input = "model quota exceeded; try again tomorrow";
        let out = sanitize_stderr(input);
        assert!(out.contains("quota exceeded"), "legit content dropped");
    }

    // ── --schema guard ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn schema_option_errors_out_immediately_with_claude_redirect() {
        let h = empty_harness();
        let opts = HarnessOptions {
            schema: Some("person_v1".to_string()),
            ..Default::default()
        };
        let cwd = std::env::temp_dir();
        let mut rx = h
            .run_prompt("ignored", &[], &cwd, None, &opts)
            .await
            .expect("run_prompt returns receiver");

        let first = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("first event must arrive");
        match first {
            HarnessEvent::Error(msg) => {
                assert!(
                    msg.contains("schema")
                        && msg.contains("gemini")
                        && msg.contains("claude")
                        && msg.contains("Try:"),
                    "error must mention schema + gemini + claude + Try hint, got: {}",
                    msg
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }

        let second = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("Done must follow Error");
        assert!(
            matches!(second, HarnessEvent::Done { .. }),
            "expected Done after Error, got {:?}",
            second
        );
    }

    // ── Harness trait basics ────────────────────────────────────────────────

    #[test]
    fn kind_is_gemini() {
        let h = empty_harness();
        assert_eq!(h.kind(), HarnessKind::Gemini);
    }

    #[test]
    fn supports_resume_is_true() {
        assert!(empty_harness().supports_resume());
    }

    #[test]
    fn set_and_get_session_id_roundtrips() {
        let h = empty_harness();
        assert!(h.get_session_id("review").is_none());
        h.set_session_id("review", "ses_xyz".to_string());
        assert_eq!(h.get_session_id("review"), Some("ses_xyz".to_string()));
    }

    // ── Arc<GeminiHarness> forwarder ───────────────────────────────────────

    #[test]
    fn arc_forwarder_delegates_kind_and_resume() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        assert_eq!(dyn_handle.kind(), HarnessKind::Gemini);
        assert!(dyn_handle.supports_resume());
    }

    #[test]
    fn arc_forwarder_delegates_session_id_mutation() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        dyn_handle.set_session_id("foo", "ses_123".into());
        assert_eq!(dyn_handle.get_session_id("foo"), Some("ses_123".into()));
        assert_eq!(inner.get_session_id("foo"), Some("ses_123".into()));
    }

    // ── Session key prefixing ───────────────────────────────────────────────

    #[test]
    fn session_key_uses_gemini_prefix() {
        let k = build_session_key(HarnessKind::Gemini, "auth");
        assert_eq!(k, "gemini:auth");
    }

    #[test]
    fn session_key_distinguishes_gemini_from_opencode() {
        let a = build_session_key(HarnessKind::Gemini, "foo");
        let b = build_session_key(HarnessKind::Opencode, "foo");
        assert_ne!(a, b, "gemini and opencode keys must not collide");
    }

    // ── HarnessKind::from_str ──────────────────────────────────────────────

    #[test]
    fn harness_kind_from_str_gemini_roundtrip() {
        let k = HarnessKind::from_str("gemini").expect("gemini parses");
        assert_eq!(k, HarnessKind::Gemini);
        assert_eq!(k.name(), "Gemini");
    }

    // ── build_argv: gemini-cli idioms ──────────────────────────────────────

    #[test]
    fn argv_basics_positional_prompt_and_stream_json() {
        let opts = HarnessOptions::default();
        let cfg = GeminiConfig::default();
        let args = build_argv("hello", None, &opts, &cfg);
        // `-o stream-json` must be present.
        let pos = args.iter().position(|a| a == "-o").expect("missing -o");
        assert_eq!(args[pos + 1], "stream-json");
        // Prompt is the last arg (positional, not `-p`).
        assert_eq!(args.last().map(|s| s.as_str()), Some("hello"));
        // `-p` is deprecated upstream — never emit it.
        assert!(!args.iter().any(|a| a == "-p"), "-p should not be emitted");
    }

    #[test]
    fn argv_session_id_uses_dash_r() {
        let opts = HarnessOptions::default();
        let cfg = GeminiConfig::default();
        let args = build_argv("hi", Some("ses_abc"), &opts, &cfg);
        let pos = args.iter().position(|a| a == "-r").expect("missing -r");
        assert_eq!(args[pos + 1], "ses_abc");
    }

    #[test]
    fn argv_bare_continue_maps_to_r_latest() {
        let opts = HarnessOptions {
            continue_last: true,
            ..Default::default()
        };
        let cfg = GeminiConfig::default();
        let args = build_argv("hi", None, &opts, &cfg);
        let pos = args.iter().position(|a| a == "-r").expect("missing -r");
        assert_eq!(
            args[pos + 1],
            "latest",
            "bare --continue should map to `-r latest`"
        );
    }

    #[test]
    fn argv_explicit_session_id_takes_precedence_over_continue_last() {
        let opts = HarnessOptions {
            continue_last: true,
            ..Default::default()
        };
        let cfg = GeminiConfig::default();
        let args = build_argv("hi", Some("ses_explicit"), &opts, &cfg);
        let pos = args.iter().position(|a| a == "-r").expect("missing -r");
        assert_eq!(args[pos + 1], "ses_explicit");
        // No duplicate `-r latest`.
        let r_count = args.iter().filter(|a| *a == "-r").count();
        assert_eq!(r_count, 1, "only one -r expected");
    }

    #[test]
    fn argv_model_option_overrides_config() {
        let opts = HarnessOptions {
            model: Some("flash-lite".into()),
            ..Default::default()
        };
        let cfg = GeminiConfig {
            model: Some("pro".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &cfg);
        let pos = args.iter().position(|a| a == "-m").expect("missing -m");
        assert_eq!(
            args[pos + 1],
            "flash-lite",
            "per-prompt model should win over config"
        );
    }

    #[test]
    fn argv_model_falls_back_to_config() {
        let opts = HarnessOptions::default();
        let cfg = GeminiConfig {
            model: Some("pro".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &cfg);
        let pos = args.iter().position(|a| a == "-m").expect("missing -m");
        assert_eq!(args[pos + 1], "pro");
    }

    #[test]
    fn argv_approval_mode_threaded_from_config() {
        let opts = HarnessOptions::default();
        let cfg = GeminiConfig {
            approval_mode: Some("yolo".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &cfg);
        let pos = args
            .iter()
            .position(|a| a == "--approval-mode")
            .expect("missing --approval-mode");
        assert_eq!(args[pos + 1], "yolo");
    }

    #[test]
    fn argv_approval_mode_option_overrides_config() {
        let opts = HarnessOptions {
            approval_mode: Some("plan".into()),
            ..Default::default()
        };
        let cfg = GeminiConfig {
            approval_mode: Some("yolo".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &cfg);
        let pos = args
            .iter()
            .position(|a| a == "--approval-mode")
            .expect("missing --approval-mode");
        assert_eq!(
            args[pos + 1],
            "plan",
            "per-prompt approval_mode should win over config"
        );
        let count = args.iter().filter(|a| *a == "--approval-mode").count();
        assert_eq!(count, 1, "only one --approval-mode expected");
    }

    // ── Panic formatter ────────────────────────────────────────────────────

    #[test]
    fn format_panic_on_str() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new("kaboom");
        assert_eq!(
            format_panic_message(&*boxed),
            "gemini: internal panic: kaboom"
        );
    }

    #[test]
    fn format_panic_on_unknown() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(
            format_panic_message(&*boxed),
            "gemini: internal panic (unknown)"
        );
    }

    // ── Ambient event emission ─────────────────────────────────────────────

    #[tokio::test]
    async fn ambient_harness_started_and_finished_broadcast_shape() {
        // Proof that the ambient-event envelope carries the Gemini name.
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(8);
        let _ = tx.send(AmbientEvent::HarnessStarted {
            harness: HarnessKind::Gemini.name().to_string(),
            run_id: "run-g1".to_string(),
            prompt_hash: None,
        });
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("channel closed");
        match ev {
            AmbientEvent::HarnessStarted {
                harness, run_id, ..
            } => {
                assert_eq!(harness, "Gemini");
                assert_eq!(run_id, "run-g1");
            }
            other => panic!("expected HarnessStarted, got {:?}", other),
        }
    }

    // ── Attachment rejection ────────────────────────────────────────────────

    #[tokio::test]
    async fn attachments_rejected_with_error_and_done() {
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = GeminiHarness::new(GeminiConfig::default(), tx);
        let att = Attachment {
            path: std::path::PathBuf::from("/tmp/terminus-nonexistent-attachment.png"),
            filename: "foo.png".into(),
            media_type: "image/png".into(),
        };
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();
        let mut rx = h
            .run_prompt("hi with an image", &[att], &cwd, None, &opts)
            .await
            .expect("run_prompt ok");

        let first = rx.recv().await.expect("expected Error");
        match first {
            HarnessEvent::Error(msg) => {
                assert!(
                    msg.contains("attachments are not yet supported"),
                    "unexpected error body: {}",
                    msg
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
        let second = rx.recv().await.expect("expected Done");
        match second {
            HarnessEvent::Done { session_id } => {
                assert!(
                    session_id.is_empty(),
                    "session_id should be empty on attachment rejection"
                );
            }
            other => panic!("expected Done, got {:?}", other),
        }
    }

    // ── Gated live-binary tests ────────────────────────────────────────────
    //
    // Run with: `TERMINUS_HAS_GEMINI=1 cargo test --release -- --ignored --test-threads=1`
    //
    // Preconditions: `gemini` on PATH, authenticated (e.g. `gemini auth login`).

    #[tokio::test]
    #[ignore]
    async fn ac1_one_shot_flash_lite_streams_and_completes() {
        if std::env::var("TERMINUS_HAS_GEMINI").is_err() {
            eprintln!("skip: TERMINUS_HAS_GEMINI not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let cfg = GeminiConfig {
            model: Some("flash-lite".into()),
            ..Default::default()
        };
        let h = GeminiHarness::new(cfg, tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();
        let mut rx = h
            .run_prompt("say hi in exactly three words", &[], &cwd, None, &opts)
            .await
            .expect("run_prompt ok");

        let mut saw_text = false;
        let mut saw_done = false;
        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
            .await
            .expect("timeout")
        {
            match ev {
                HarnessEvent::Text(s) if !s.is_empty() => saw_text = true,
                HarnessEvent::Done { .. } => {
                    saw_done = true;
                    break;
                }
                HarnessEvent::Error(e) => panic!("harness error: {}", e),
                _ => {}
            }
        }
        assert!(saw_text, "at least one Text event expected");
        assert!(saw_done, "Done event expected");
    }

    #[tokio::test]
    #[ignore]
    async fn ac2_interactive_two_prompts_reuse_session() {
        if std::env::var("TERMINUS_HAS_GEMINI").is_err() {
            eprintln!("skip: TERMINUS_HAS_GEMINI not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = GeminiHarness::new(GeminiConfig::default(), tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();

        let mut rx1 = h
            .run_prompt("say hello", &[], &cwd, None, &opts)
            .await
            .expect("run_prompt ok");

        let mut captured_session_id: Option<String> = None;
        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(120), rx1.recv())
            .await
            .expect("timeout on first prompt")
        {
            match ev {
                HarnessEvent::Done { session_id } if !session_id.is_empty() => {
                    captured_session_id = Some(session_id);
                    break;
                }
                HarnessEvent::Done { .. } => break,
                HarnessEvent::Error(e) => panic!("first prompt error: {}", e),
                _ => {}
            }
        }

        let sid = captured_session_id.expect("first prompt should yield a session_id");

        let mut rx2 = h
            .run_prompt("say goodbye", &[], &cwd, Some(&sid), &opts)
            .await
            .expect("run_prompt ok");

        let mut second_session_id: Option<String> = None;
        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(120), rx2.recv())
            .await
            .expect("timeout on second prompt")
        {
            match ev {
                HarnessEvent::Done { session_id } => {
                    second_session_id = Some(session_id);
                    break;
                }
                HarnessEvent::Error(e) => panic!("second prompt error: {}", e),
                _ => {}
            }
        }

        let sid2 = second_session_id.expect("second prompt should yield a session_id");
        assert_eq!(
            sid, sid2,
            "both prompts should report the same gemini sessionID"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn ac3_bogus_session_id_surfaces_error_path() {
        if std::env::var("TERMINUS_HAS_GEMINI").is_err() {
            eprintln!("skip: TERMINUS_HAS_GEMINI not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = GeminiHarness::new(GeminiConfig::default(), tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();
        let mut rx = h
            .run_prompt("hi", &[], &cwd, Some("ses_does_not_exist"), &opts)
            .await
            .expect("run_prompt ok");

        let mut saw_error_or_done = false;
        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
            .await
            .expect("timeout")
        {
            match ev {
                HarnessEvent::Error(_) | HarnessEvent::Done { .. } => {
                    saw_error_or_done = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_error_or_done, "expected a clean error/Done path");
    }

    #[tokio::test]
    #[ignore]
    async fn ac4_tool_use_visibility_with_approval_yolo() {
        if std::env::var("TERMINUS_HAS_GEMINI").is_err() {
            eprintln!("skip: TERMINUS_HAS_GEMINI not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let cfg = GeminiConfig {
            model: Some("flash".into()),
            approval_mode: Some("yolo".into()),
            ..Default::default()
        };
        let h = GeminiHarness::new(cfg, tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();
        let mut rx = h
            .run_prompt(
                "read the first line of /etc/hostname and reply with it verbatim",
                &[],
                &cwd,
                None,
                &opts,
            )
            .await
            .expect("run_prompt ok");

        let mut saw_tool_use = false;
        let mut saw_done = false;
        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(180), rx.recv())
            .await
            .expect("timeout")
        {
            match ev {
                HarnessEvent::ToolUse { .. } => saw_tool_use = true,
                HarnessEvent::Done { .. } => {
                    saw_done = true;
                    break;
                }
                HarnessEvent::Error(e) => panic!("harness error: {}", e),
                _ => {}
            }
        }
        assert!(
            saw_tool_use,
            "a tool-using prompt should yield at least one paired ToolUse event"
        );
        assert!(saw_done, "Done event expected");
    }
}
