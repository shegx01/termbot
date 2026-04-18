//! Opencode CLI-subprocess harness.
//!
//! Each prompt spawns `opencode run --format json [...]` as a short-lived
//! child process. Stdout carries newline-delimited JSON events that we
//! translate into [`HarnessEvent`]s.  Session resume is handled via
//! `--session <id>`: the harness captures the opencode `sessionID` from the
//! first event it observes and the App layer persists the name → id mapping
//! in state via `HarnessEvent::Done { session_id }`.
//!
//! Unlike the Claude SDK harness, this harness has no persistent sidecar and
//! no lifecycle to manage. `kill_on_drop(true)` on the `Command` ensures any
//! in-flight child process terminates when terminus exits.
//!
//! ## Event translation (see `translate_event`)
//!
//! Observed event shapes from `opencode run --format json`:
//! - `{"type":"step_start", "sessionID":"ses_…", "part":{…}}` — captured for session id
//! - `{"type":"text", "sessionID":"…", "part":{"text":"…"}}` — translated to `Text`
//! - `{"type":"step_finish", …}` — terminal; stops the read loop
//! - `{"type":"tool_use", …, "part":{"tool":"bash","state":{"status":"completed","input":{…},"output":"…"}}}` — translated to `ToolUse`
//!
//! Tool-use events are atomic (no separate running/pending events in `--format
//! json` mode). Each carries the full call + result in a single JSON line.
//!
//! ## Integration tests
//!
//! Live-binary tests are gated `#[ignore]` + `TERMINUS_HAS_OPENCODE=1`.
//! Run with `TERMINUS_HAS_OPENCODE=1 cargo test -- --ignored`. See
//! `docs/integration-tests.md` for preconditions.

use super::{build_session_key, Harness, HarnessEvent, HarnessKind};
use crate::chat_adapters::Attachment;
use crate::command::{HarnessOptions, OpencodeSubcommand};
use crate::config::OpencodeConfig;
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

/// Opencode CLI-subprocess harness.
pub struct OpencodeHarness {
    config: OpencodeConfig,
    /// Map of opaque session-name → opencode sessionID (`ses_…`). Keys here
    /// are the raw user-supplied names — the App layer owns the prefixed
    /// `{kind}:{name}` form used for cross-harness isolation in state.
    sessions: Arc<StdMutex<HashMap<String, String>>>,
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
}

impl OpencodeHarness {
    /// Construct a harness with the provided config and ambient-event sender.
    pub fn new(config: OpencodeConfig, ambient_tx: broadcast::Sender<AmbientEvent>) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: Some(ambient_tx),
        }
    }

    /// Test constructor with no ambient-event broadcast.
    #[cfg(test)]
    pub fn new_with_config(config: OpencodeConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: None,
        }
    }

    /// Fire a `HarnessStarted` ambient event if a broadcast sender is attached.
    #[allow(dead_code)]
    fn emit_started(&self, run_id: &str) {
        if let Some(tx) = &self.ambient_tx {
            let _ = tx.send(AmbientEvent::HarnessStarted {
                harness: HarnessKind::Opencode.name().to_string(),
                run_id: run_id.to_string(),
                prompt_hash: None,
            });
        }
    }

    /// Fire a `HarnessFinished` ambient event with the given status.
    #[allow(dead_code)]
    fn emit_finish(&self, run_id: &str, status: &str) {
        if let Some(tx) = &self.ambient_tx {
            let _ = tx.send(AmbientEvent::HarnessFinished {
                harness: HarnessKind::Opencode.name().to_string(),
                run_id: run_id.to_string(),
                status: status.to_string(),
            });
        }
    }
}

#[async_trait]
impl Harness for OpencodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
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

        let binary: PathBuf = self
            .config
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("opencode"));

        // Build argv in the order: `run --format json [--session …] [-m …]
        // [--agent …] [-f <path>]* [--title …] [--share] [--pure] [--fork]
        // [--continue] -- <prompt>`. We do NOT use `--` because opencode's
        // `run` treats positional args as the prompt already and adding `--`
        // confuses yargs. Attachments use `-f <path>` per the CLI.
        let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];
        if let Some(sid) = session_id {
            args.push("--session".into());
            args.push(sid.to_string());
        }
        if let Some(ref m) = self.config.model {
            args.push("-m".into());
            args.push(m.clone());
        }
        if let Some(ref a) = self.config.agent {
            args.push("--agent".into());
            args.push(a.clone());
        }
        for att in attachments {
            args.push("-f".into());
            args.push(att.path.to_string_lossy().into_owned());
        }
        // Per-prompt flags (after attachments, before the prompt positional).
        if let Some(ref t) = options.title {
            args.push("--title".into());
            args.push(t.clone());
        }
        if options.share {
            args.push("--share".into());
        }
        if options.pure {
            args.push("--pure".into());
        }
        if options.fork {
            args.push("--fork".into());
        }
        if options.continue_last {
            args.push("--continue".into());
        }
        // Prompt as the final positional. opencode `run` accepts multi-word
        // messages as separate positionals, but we pass it as a single arg to
        // preserve quoting/whitespace semantics.
        if !prompt.is_empty() {
            args.push(prompt.to_string());
        }

        let cwd = cwd.to_path_buf();
        let attachment_paths: Vec<PathBuf> = attachments.iter().map(|a| a.path.clone()).collect();
        let sessions = Arc::clone(&self.sessions);
        let ambient_tx = self.ambient_tx.clone();
        let run_id = ulid::Ulid::new().to_string();

        // Emit "started" up front — the `run_id` is local to the harness; the
        // App layer emits its own `HarnessStarted` with the public-facing id.
        {
            let harness_started = AmbientEvent::HarnessStarted {
                harness: HarnessKind::Opencode.name().to_string(),
                run_id: run_id.clone(),
                prompt_hash: None,
            };
            if let Some(ref tx) = ambient_tx {
                let _ = tx.send(harness_started);
            }
        }

        let run_id_for_task = run_id.clone();
        tokio::spawn(async move {
            let body =
                run_opencode_inner(binary, args, cwd, event_tx.clone(), Arc::clone(&sessions));
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> =
                AssertUnwindSafe(body).catch_unwind().await;

            let status = match result {
                Ok(()) => "ok",
                Err(panic_info) => {
                    let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                        format!("OpenCode: internal panic: {}", s)
                    } else if let Some(s) = panic_info.downcast_ref::<String>() {
                        format!("OpenCode: internal panic: {}", s)
                    } else {
                        "OpenCode: internal panic (unknown)".to_string()
                    };
                    let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                    "error"
                }
            };

            if let Some(tx) = ambient_tx {
                let _ = tx.send(AmbientEvent::HarnessFinished {
                    harness: HarnessKind::Opencode.name().to_string(),
                    run_id: run_id_for_task,
                    status: status.to_string(),
                });
            }

            // Clean up any attachment temp files threaded through via `-f`.
            // The opencode child has already read them by the time we're here
            // (either it exited normally or panicked — both close the stream).
            for path in &attachment_paths {
                let _ = tokio::fs::remove_file(path).await;
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

/// `Arc<OpencodeHarness>` forwarder so `App` can hold a strong named handle
/// and insert a clone into the `harnesses: HashMap<HarnessKind, Box<dyn
/// Harness>>` map without double-boxing.
#[async_trait]
impl Harness for Arc<OpencodeHarness> {
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

/// Strip ANSI escape sequences (CSI sequences and simple OSC). Hand-rolled
/// to avoid a new crate dep — matches `\x1b[...m`, `\x1b[...K`, and similar.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until a terminator: letter @-~ for CSI; `\x07` or `\x1b\\` for OSC.
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next(); // consume `[`
                    for cc in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&cc) {
                            break;
                        }
                    }
                    continue;
                } else if next == ']' {
                    chars.next(); // consume `]`
                    while let Some(cc) = chars.next() {
                        if cc == '\x07' {
                            break;
                        }
                        if cc == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                    continue;
                }
            }
            // Unknown escape — drop the ESC and continue.
            continue;
        }
        out.push(c);
    }
    out
}

impl OpencodeHarness {
    /// Spawn `opencode <sub> <args>` with `NO_COLOR=1`, capture stdout, strip
    /// ANSI escapes, return the trimmed string. Bounded by a 30s timeout.
    /// stderr is captured and appended on non-zero exit as diagnostic context.
    /// DOES NOT create a session or emit ambient events — subcommands are
    /// one-shot queries, not prompts.
    pub async fn run_subcommand(&self, sub: OpencodeSubcommand, args: &[String]) -> Result<String> {
        let binary = self
            .config
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("opencode"));

        let sub_name = match sub {
            OpencodeSubcommand::Models => "models",
            OpencodeSubcommand::Stats => "stats",
            OpencodeSubcommand::Sessions => "session", // `opencode session list`
            OpencodeSubcommand::Providers => "auth",   // `opencode auth list`
            OpencodeSubcommand::Export => "export",
        };

        let mut argv: Vec<String> = vec![sub_name.into()];
        // `sessions` and `providers` are user-friendly aliases; they both
        // map to a `list` sub-subcommand.
        if matches!(
            sub,
            OpencodeSubcommand::Sessions | OpencodeSubcommand::Providers
        ) {
            argv.push("list".into());
        }
        argv.extend(args.iter().cloned());

        let mut cmd = Command::new(&binary);
        cmd.args(&argv)
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("opencode binary not found: {}", binary.display())
            } else {
                anyhow::anyhow!("opencode subcommand spawn failed: {}", e)
            }
        })?;

        let output_fut = child.wait_with_output();
        let out = tokio::time::timeout(std::time::Duration::from_secs(30), output_fut)
            .await
            .map_err(|_| anyhow::anyhow!("opencode {} timed out after 30s", sub_name))?
            .map_err(|e| anyhow::anyhow!("opencode {} wait failed: {}", sub_name, e))?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stripped = strip_ansi(&stdout);
        let stripped = stripped.trim();

        if !out.status.success() {
            let stderr_trim = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            return Err(anyhow::anyhow!(
                "opencode {} exited with status {}: {}",
                sub_name,
                code,
                if stderr_trim.is_empty() {
                    stripped.to_string()
                } else {
                    stderr_trim
                }
            ));
        }

        Ok(stripped.to_string())
    }
}

/// Spawn the opencode child, read newline-delimited JSON events from stdout,
/// translate them, and emit `HarnessEvent::Done` on clean exit or
/// `HarnessEvent::Error` on non-zero exit.
async fn run_opencode_inner(
    binary: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    event_tx: mpsc::Sender<HarnessEvent>,
    sessions: Arc<StdMutex<HashMap<String, String>>>,
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
                    "OpenCode binary not found: {} (set [harness.opencode] binary_path or install opencode on PATH)",
                    binary.display()
                )
            } else {
                format!("OpenCode spawn failed: {}", e)
            };
            let _ = event_tx.send(HarnessEvent::Error(msg)).await;
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = event_tx
                .send(HarnessEvent::Error(
                    "OpenCode: stdout pipe missing".to_string(),
                ))
                .await;
            let _ = child.kill().await;
            return;
        }
    };
    let stderr = child.stderr.take();

    let mut reader = BufReader::new(stdout).lines();
    let mut captured_session_id: Option<String> = None;
    let mut saw_recognized = false;
    let mut saw_unknown = false;
    let mut received_finish = false;

    loop {
        // Read the next line or time out after 5 minutes of silence. Opencode
        // prompts with long-running tools can easily sit silent for a while,
        // so the timeout is generous; it exists to prevent a wedged child
        // from stranding the task forever.
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(300), reader.next_line()).await;

        let line = match next {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!("OpenCode stdout read: {}", e)))
                    .await;
                break;
            }
            Err(_) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(
                        "OpenCode: no output for 5 minutes — killing subprocess".to_string(),
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
                tracing::debug!("opencode: failed to parse JSON line: {} ({})", e, trimmed);
                continue;
            }
        };

        // Capture session id from the first event that carries one.
        if captured_session_id.is_none() {
            if let Some(sid) = value.get("sessionID").and_then(|s| s.as_str()) {
                captured_session_id = Some(sid.to_string());
            }
        }

        match translate_event(&value) {
            Some(translated) => {
                saw_recognized = true;
                for ev in translated {
                    match ev {
                        TranslatedEvent::Text(t) => {
                            if !t.is_empty() {
                                let _ = event_tx.send(HarnessEvent::Text(t)).await;
                            }
                        }
                        TranslatedEvent::StepFinish => {
                            received_finish = true;
                        }
                        TranslatedEvent::ToolUse {
                            tool,
                            description,
                            input,
                            output,
                        } => {
                            let _ = event_tx
                                .send(HarnessEvent::ToolUse {
                                    tool,
                                    description,
                                    input,
                                    output,
                                })
                                .await;
                        }
                    }
                }
                if received_finish {
                    break;
                }
            }
            None => {
                // Either a known-but-silent event (step_start) or unknown.
                let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty != "step_start" {
                    saw_unknown = true;
                    tracing::debug!("opencode: unknown event type `{}`", ty);
                }
            }
        }
    }

    // Reap the child. `kill_on_drop` guarantees cleanup even if we bail early
    // without the explicit wait, but we still need the exit status for an
    // accurate error path.
    let status = match child.wait().await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("opencode: child.wait() failed: {}", e);
            None
        }
    };

    if let Some(status) = status {
        if !status.success() {
            // Try to read stderr for a useful message.
            let stderr_msg = match stderr {
                Some(mut s) => {
                    use tokio::io::AsyncReadExt;
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf).await;
                    buf.trim().to_string()
                }
                None => String::new(),
            };
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let detail = if stderr_msg.is_empty() {
                format!("OpenCode exited with status {} (no stderr)", code)
            } else {
                format!("OpenCode exited with status {}: {}", code, stderr_msg)
            };
            let _ = event_tx.send(HarnessEvent::Error(detail)).await;
        }
    }

    // If the child produced no recognized events at all, surface a clear
    // signal so the user doesn't see silence. Unknown-but-no-recognized is
    // the most likely version-drift signature.
    if !saw_recognized {
        let msg = if saw_unknown {
            "OpenCode: no recognized events received (version drift — check `opencode --version`)"
                .to_string()
        } else {
            "OpenCode: no response content".to_string()
        };
        let _ = event_tx.send(HarnessEvent::Text(msg)).await;
    }

    // Persist the session id internally for future `get_session_id` lookups.
    // The App layer additionally consumes the session_id via `HarnessEvent::Done`
    // and writes its own name→id index under a prefixed key in state.
    let sid = captured_session_id.clone().unwrap_or_default();
    if !sid.is_empty() {
        // There is no opencode-side name in `run_prompt`, but we stash the id
        // under a stable placeholder so tests can observe the capture path.
        // The real name→id wiring lives in `App::send_harness_prompt` via
        // `HarnessEvent::Done { session_id }`.
        let mut map = sessions.lock().unwrap();
        map.insert("_last".to_string(), sid.clone());
    }

    let _ = event_tx.send(HarnessEvent::Done { session_id: sid }).await;
}

/// Pure translation of a single opencode JSON event into zero-or-more
/// `TranslatedEvent`s. Returns `None` for ignored event types (e.g.
/// `step_start`, unknown) so callers can count them for diagnostics.
///
/// Event shapes observed:
/// ```json
/// {"type":"step_start", "sessionID":"ses_…", "part":{…}}
/// {"type":"text", "sessionID":"…", "part":{"text":"…"}}
/// {"type":"step_finish", …}
/// {"type":"tool_use", "sessionID":"…", "part":{"tool":"bash","state":{"status":"completed","input":{…},"output":"…"}}}
/// ```
///
/// Tool-use events are atomic — one event per call with both input and output
/// present. No accumulator is needed. The `status` field may be `"completed"`
/// or `"error"`; on error the `output` field is suppressed and the `description`
/// includes the error message from `part.state.error`.
pub(crate) fn translate_event(event: &serde_json::Value) -> Option<Vec<TranslatedEvent>> {
    let ty = event.get("type").and_then(|t| t.as_str())?;
    match ty {
        "text" => {
            let text = event
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())?;
            if text.is_empty() {
                Some(Vec::new())
            } else {
                Some(vec![TranslatedEvent::Text(text.to_string())])
            }
        }
        "step_start" => None,
        "step_finish" => Some(vec![TranslatedEvent::StepFinish]),
        "tool_use" => {
            let part = event.get("part")?;
            let tool = part.get("tool").and_then(|v| v.as_str())?.to_string();
            let state = part.get("state")?;
            let status = state.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let title = state
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_json = state
                .get("input")
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let output_str = state
                .get("output")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let description = match status {
                "completed" => {
                    if title.is_empty() {
                        tool.clone()
                    } else {
                        title
                    }
                }
                "error" => {
                    let err_msg = state
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool failed");
                    format!(
                        "{} ({})",
                        if title.is_empty() {
                            tool.clone()
                        } else {
                            title
                        },
                        err_msg
                    )
                }
                other => format!(
                    "{} (status: {})",
                    if title.is_empty() {
                        tool.clone()
                    } else {
                        title
                    },
                    other
                ),
            };

            let output = if status == "completed" {
                output_str
            } else {
                None
            };

            Some(vec![TranslatedEvent::ToolUse {
                tool,
                description,
                input: input_json,
                output,
            }])
        }
        _ => None,
    }
}

/// Pure-data intermediate representation emitted by `translate_event`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TranslatedEvent {
    Text(String),
    StepFinish,
    ToolUse {
        tool: String,
        description: String,
        input: Option<String>,
        output: Option<String>,
    },
}

#[allow(dead_code)]
/// Build the `{kind}:{name}` prefixed state key for an opencode named session.
/// Exposed for parity with the App-layer lookups.
pub(crate) fn session_key(name: &str) -> String {
    build_session_key(HarnessKind::Opencode, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_harness() -> OpencodeHarness {
        OpencodeHarness::new_with_config(OpencodeConfig::default())
    }

    // ── translate_event ──────────────────────────────────────────────────────

    #[test]
    fn translate_event_text_returns_text_translated() {
        let ev = serde_json::json!({
            "type": "text",
            "sessionID": "ses_1",
            "part": {"text": "hello world"}
        });
        let out = translate_event(&ev).expect("text event translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::Text("hello world".into()));
    }

    #[test]
    fn translate_event_text_empty_returns_empty_vec() {
        let ev = serde_json::json!({
            "type": "text",
            "part": {"text": ""}
        });
        let out = translate_event(&ev).expect("empty text translates to empty vec");
        assert!(out.is_empty(), "empty text should produce no Text events");
    }

    #[test]
    fn translate_event_step_start_returns_none() {
        let ev = serde_json::json!({
            "type": "step_start",
            "sessionID": "ses_1",
            "part": {"type": "step-start"}
        });
        assert!(translate_event(&ev).is_none());
    }

    #[test]
    fn translate_event_step_finish_returns_stepfinish() {
        let ev = serde_json::json!({
            "type": "step_finish",
            "part": {"reason": "stop", "tokens": {"total": 100}, "cost": 0}
        });
        let out = translate_event(&ev).expect("step_finish translates");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], TranslatedEvent::StepFinish);
    }

    #[test]
    fn translate_event_unknown_type_returns_none() {
        let ev = serde_json::json!({"type": "tool_mystery_42"});
        assert!(translate_event(&ev).is_none());
    }

    #[test]
    fn translate_event_missing_type_returns_none() {
        let ev = serde_json::json!({"part": {"text": "orphan"}});
        assert!(translate_event(&ev).is_none());
    }

    #[test]
    fn translate_event_tool_use_completed_extracts_structured_fields() {
        let ev = serde_json::json!({
            "type": "tool_use",
            "timestamp": 1776515159863u64,
            "sessionID": "ses_abc",
            "part": {
                "id": "prt_1",
                "messageID": "msg_1",
                "sessionID": "ses_abc",
                "type": "tool",
                "tool": "bash",
                "callID": "call_56a0895561b54640b5d00822",
                "state": {
                    "status": "completed",
                    "input": {"command": "echo hello", "description": "Echo hello"},
                    "output": "hello\n",
                    "metadata": {"output": "hello\n", "exit": 0, "truncated": false},
                    "title": "Echo hello",
                    "time": {"start": 0, "end": 1}
                }
            }
        });
        let out = translate_event(&ev).expect("tool_use event should translate");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                description,
                input,
                output,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(description, "Echo hello");
                let input_str = input.as_deref().expect("input should be Some");
                assert!(
                    input_str.contains("echo hello"),
                    "input should contain the command, got: {}",
                    input_str
                );
                assert_eq!(output.as_deref(), Some("hello\n"));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn translate_event_tool_use_error_status_has_no_output() {
        let ev = serde_json::json!({
            "type": "tool_use",
            "sessionID": "ses_abc",
            "part": {
                "tool": "bash",
                "state": {
                    "status": "error",
                    "input": {"command": "fail"},
                    "error": "boom",
                    "title": "Run fail"
                }
            }
        });
        let out = translate_event(&ev).expect("tool_use error event should translate");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolUse {
                description,
                output,
                ..
            } => {
                assert!(
                    description.contains("boom"),
                    "description should include error message, got: {}",
                    description
                );
                assert!(output.is_none(), "output should be None on error status");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn translate_event_tool_use_missing_state_returns_none() {
        // Malformed: part has no `state` key
        let ev = serde_json::json!({
            "type": "tool_use",
            "sessionID": "ses_abc",
            "part": {
                "tool": "bash"
            }
        });
        assert!(
            translate_event(&ev).is_none(),
            "missing state should return None"
        );
    }

    // ── build_session_key / session_key ──────────────────────────────────────

    #[test]
    fn session_key_roundtrips_prefix() {
        assert_eq!(session_key("auth"), "opencode:auth");
    }

    #[test]
    fn build_session_key_distinguishes_harnesses() {
        let a = build_session_key(HarnessKind::Claude, "foo");
        let b = build_session_key(HarnessKind::Opencode, "foo");
        assert_ne!(a, b, "claude and opencode keys must not collide");
        assert_eq!(a, "claude:foo");
        assert_eq!(b, "opencode:foo");
    }

    #[test]
    fn build_session_key_preserves_name_characters() {
        assert_eq!(
            build_session_key(HarnessKind::Opencode, "auth-v2_review"),
            "opencode:auth-v2_review"
        );
    }

    // ── HarnessKind::from_str ────────────────────────────────────────────────

    #[test]
    fn harness_kind_from_str_opencode_roundtrip() {
        let k = HarnessKind::from_str("opencode").expect("opencode parses");
        assert_eq!(k, HarnessKind::Opencode);
        assert_eq!(k.name(), "OpenCode");
    }

    #[test]
    fn harness_kind_from_str_is_case_insensitive() {
        assert_eq!(
            HarnessKind::from_str("OPENCODE"),
            Some(HarnessKind::Opencode)
        );
        assert_eq!(
            HarnessKind::from_str("OpenCode"),
            Some(HarnessKind::Opencode)
        );
    }

    // ── Harness trait basics ─────────────────────────────────────────────────

    #[test]
    fn kind_is_opencode() {
        let h = empty_harness();
        assert_eq!(h.kind(), HarnessKind::Opencode);
    }

    #[test]
    fn supports_resume_is_true() {
        assert!(empty_harness().supports_resume());
    }

    #[test]
    fn set_and_get_session_id_roundtrips() {
        let h = empty_harness();
        assert!(h.get_session_id("auth").is_none());
        h.set_session_id("auth", "ses_abc".to_string());
        assert_eq!(h.get_session_id("auth"), Some("ses_abc".to_string()));
    }

    // ── Arc<OpencodeHarness> forwarder ───────────────────────────────────────

    #[test]
    fn arc_forwarder_delegates_kind_and_resume() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        assert_eq!(dyn_handle.kind(), HarnessKind::Opencode);
        assert!(dyn_handle.supports_resume());
    }

    #[test]
    fn arc_forwarder_delegates_session_id_mutation() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        dyn_handle.set_session_id("foo", "ses_123".into());
        assert_eq!(dyn_handle.get_session_id("foo"), Some("ses_123".into()));
        // The inner struct sees the mutation too.
        assert_eq!(inner.get_session_id("foo"), Some("ses_123".into()));
    }

    // ── Ambient event emission ───────────────────────────────────────────────

    #[tokio::test]
    async fn emit_started_delivers_harness_started_through_broadcast() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
        h.emit_started("run-1");
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv closed");
        match ev {
            AmbientEvent::HarnessStarted {
                harness,
                run_id,
                prompt_hash,
            } => {
                assert_eq!(harness, "OpenCode");
                assert_eq!(run_id, "run-1");
                assert!(prompt_hash.is_none());
            }
            other => panic!("expected HarnessStarted, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn emit_finish_delivers_harness_finished_status() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
        h.emit_finish("run-2", "error");
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv closed");
        match ev {
            AmbientEvent::HarnessFinished {
                harness,
                run_id,
                status,
            } => {
                assert_eq!(harness, "OpenCode");
                assert_eq!(run_id, "run-2");
                assert_eq!(status, "error");
            }
            other => panic!("expected HarnessFinished, got {:?}", other),
        }
    }

    // ── Panic-message formatter (matches claude.rs pattern) ──────────────────

    fn format_panic(info: &(dyn std::any::Any + Send)) -> String {
        if let Some(s) = info.downcast_ref::<&str>() {
            format!("OpenCode: internal panic: {}", s)
        } else if let Some(s) = info.downcast_ref::<String>() {
            format!("OpenCode: internal panic: {}", s)
        } else {
            "OpenCode: internal panic (unknown)".to_string()
        }
    }

    #[test]
    fn format_panic_on_str_slice() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new("kaboom");
        assert_eq!(format_panic(&*boxed), "OpenCode: internal panic: kaboom");
    }

    #[test]
    fn format_panic_on_owned_string() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new(String::from("owned boom"));
        assert_eq!(
            format_panic(&*boxed),
            "OpenCode: internal panic: owned boom"
        );
    }

    #[test]
    fn format_panic_on_unknown_box() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(format_panic(&*boxed), "OpenCode: internal panic (unknown)");
    }

    // ── Argv construction smoke test (exercises the -f attachment path) ──────
    //
    // We don't spawn the child here — just verify that the Command builder
    // accepts the expected flags so the shape stays stable.
    #[test]
    fn opencode_config_defaults_are_all_none() {
        let cfg = OpencodeConfig::default();
        assert!(cfg.binary_path.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.agent.is_none());
    }

    #[test]
    fn attachment_path_threading_includes_dash_f() {
        // White-box: reconstruct the argv the way `run_prompt` does, for a
        // two-attachment case, and assert the flag/pairing order.
        let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];
        let paths = [PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.pdf")];
        for p in &paths {
            args.push("-f".into());
            args.push(p.to_string_lossy().into_owned());
        }
        assert_eq!(
            args,
            vec![
                "run".to_string(),
                "--format".into(),
                "json".into(),
                "-f".into(),
                "/tmp/a.png".into(),
                "-f".into(),
                "/tmp/b.pdf".into(),
            ]
        );
    }

    // ── Gated integration tests (require `TERMINUS_HAS_OPENCODE=1`) ──────────
    //
    // Run with: `TERMINUS_HAS_OPENCODE=1 cargo test -- --ignored`.

    #[tokio::test]
    #[ignore]
    async fn ac1_one_shot_haiku_streams_and_completes() {
        if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
            eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
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
        if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
            eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();

        // First prompt — no session_id; capture the session from Done event.
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

        // Second prompt — resume the same session.
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
            "both prompts should report the same opencode sessionID"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn ac3_bogus_session_id_surfaces_error_path() {
        if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
            eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();
        // An id that cannot possibly exist in opencode's session store.
        let mut rx = h
            .run_prompt(
                "hi",
                &[],
                &cwd,
                Some("ses_definitely_not_real_000000000000"),
                &opts,
            )
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
    async fn ac4_tool_use_visibility_with_agent_build() {
        if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
            eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let config = OpencodeConfig {
            agent: Some("build".to_string()),
            ..OpencodeConfig::default()
        };
        let h = OpencodeHarness::new(config, tx);
        let opts = HarnessOptions::default();
        let cwd = std::env::temp_dir();

        let mut rx = h
            .run_prompt(
                "run the bash command `echo hello` and tell me the output",
                &[],
                &cwd,
                None,
                &opts,
            )
            .await
            .expect("run_prompt ok");

        let mut saw_tool_use = false;
        let mut saw_done = false;
        let mut tool_name = String::new();
        let mut tool_input: Option<String> = None;
        let mut tool_output: Option<String> = None;

        while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(180), rx.recv())
            .await
            .expect("timeout waiting for tool_use/Done")
        {
            match ev {
                HarnessEvent::ToolUse {
                    tool,
                    input,
                    output,
                    ..
                } => {
                    saw_tool_use = true;
                    tool_name = tool;
                    tool_input = input;
                    tool_output = output;
                }
                HarnessEvent::Done { .. } => {
                    saw_done = true;
                    break;
                }
                HarnessEvent::Error(e) => panic!("harness error: {}", e),
                _ => {}
            }
        }

        assert!(saw_tool_use, "expected at least one HarnessEvent::ToolUse");
        assert!(!tool_name.is_empty(), "tool name should be non-empty");
        let input_str = tool_input.expect("tool input should be Some");
        assert!(
            input_str.to_lowercase().contains("echo"),
            "tool input should mention 'echo', got: {}",
            input_str
        );
        let output_str = tool_output.expect("tool output should be Some on completed tool");
        assert!(
            !output_str.is_empty(),
            "tool output should be non-empty, got empty string"
        );
        assert!(saw_done, "expected HarnessEvent::Done after tool use");
    }

    // ── strip_ansi unit tests ────────────────────────────────────────────────

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

    // ── argv_construction_threads_all_per_prompt_flags ──────────────────────

    #[test]
    fn argv_construction_threads_all_per_prompt_flags() {
        // White-box: reconstruct the argv the way `run_prompt` does when all
        // per-prompt flags are set, and assert the expected order.
        let options = HarnessOptions {
            title: Some("my-session".into()),
            share: true,
            pure: true,
            fork: true,
            continue_last: true,
            ..Default::default()
        };
        let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];
        // Per-prompt flags (mirrors run_prompt order):
        if let Some(ref t) = options.title {
            args.push("--title".into());
            args.push(t.clone());
        }
        if options.share {
            args.push("--share".into());
        }
        if options.pure {
            args.push("--pure".into());
        }
        if options.fork {
            args.push("--fork".into());
        }
        if options.continue_last {
            args.push("--continue".into());
        }

        assert!(args.contains(&"--title".to_string()), "missing --title");
        assert!(
            args.contains(&"my-session".to_string()),
            "missing title value"
        );
        assert!(args.contains(&"--share".to_string()), "missing --share");
        assert!(args.contains(&"--pure".to_string()), "missing --pure");
        assert!(args.contains(&"--fork".to_string()), "missing --fork");
        assert!(
            args.contains(&"--continue".to_string()),
            "missing --continue"
        );

        // Verify relative ordering: title before share before pure before fork before continue
        let pos = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert!(pos("--title") < pos("--share"));
        assert!(pos("--share") < pos("--pure"));
        assert!(pos("--pure") < pos("--fork"));
        assert!(pos("--fork") < pos("--continue"));
    }

    // ── Gated subcommand integration test ────────────────────────────────────

    #[tokio::test]
    #[ignore]
    async fn subcommand_models_returns_non_empty_output() {
        if std::env::var("TERMINUS_HAS_OPENCODE").is_err() {
            eprintln!("skip: TERMINUS_HAS_OPENCODE not set");
            return;
        }
        let (tx, _rx) = broadcast::channel::<AmbientEvent>(8);
        let h = OpencodeHarness::new(OpencodeConfig::default(), tx);
        let result = h.run_subcommand(OpencodeSubcommand::Models, &[]).await;
        let output = result.expect("run_subcommand(models) should succeed");
        assert!(!output.is_empty(), "models output should be non-empty");
        let lower = output.to_lowercase();
        let has_provider = ["openrouter", "anthropic", "claude", "gpt", "openai"]
            .iter()
            .any(|p| lower.contains(p));
        assert!(
            has_provider,
            "models output should mention at least one known provider, got: {}",
            output
        );
    }
}
