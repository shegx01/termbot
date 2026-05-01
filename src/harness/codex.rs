//! Codex CLI-subprocess harness.
//!
//! Wraps OpenAI's `codex` CLI (verified against codex-cli 0.128.0) — each
//! prompt spawns `codex exec --json` (or `codex exec resume --json …` for
//! continuation) as a short-lived child process. Mirrors the structure of
//! [`crate::harness::gemini`] and [`crate::harness::opencode`] with codex-
//! specific argv construction and event translation.
//!
//! ## Codex 0.128.0 ground truth (verified by spike)
//!
//! - **Approval flag absent on `exec`.** `codex exec` does not accept
//!   `--ask-for-approval` (it's a top-level-only flag). `codex exec` is
//!   non-interactive by default in 0.128, so no replacement is needed —
//!   the harness just pins the sandbox explicitly with `-s <value>`.
//! - **`--full-auto` is deprecated.** Codex 0.128 prints
//!   `"--full-auto is deprecated; use --sandbox workspace-write instead"`
//!   on stderr if used. Terminus emits `-s workspace-write` instead (or
//!   the user's per-prompt / config sandbox value when set), avoiding both
//!   the deprecation warning and the future-removal risk.
//! - **`--skip-git-repo-check`** is always passed because terminus's tmux cwd
//!   may not be a git repo.
//! - **`--ephemeral`** is passed when no named session is in play, so one-shot
//!   prompts don't pollute codex's session log.
//! - **stdin must be `Stdio::null()`** — `codex exec` reads stdin even when
//!   the prompt is supplied as a positional arg, and the read blocks forever
//!   if a TTY isn't attached.
//! - **`--ignore-user-config`** is opt-in via `[harness.codex] ignore_user_config`.
//! - **Default model is `gpt-5.5`** as of codex 0.128 (priority 0 in the
//!   bundled model manifest). `gpt-5.4` and `gpt-5.4-mini` remain available;
//!   `gpt-5.3-codex` was removed.
//!
//! ## Event schema (NDJSON, one JSON object per line)
//!
//! ```text
//! {"type":"thread.started","thread_id":"<UUIDv7>"}
//! {"type":"turn.started"}
//! {"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"…"}}
//! {"type":"item.started","item":{"id":"item_1","type":"command_execution",
//!   "command":"…","aggregated_output":"","exit_code":null,"status":"in_progress"}}
//! {"type":"item.completed","item":{"id":"item_1","type":"command_execution",
//!   "command":"…","aggregated_output":"…","exit_code":0,"status":"completed"}}
//! {"type":"turn.completed","usage":{"input_tokens":…,"output_tokens":…,…}}
//! ```
//!
//! `agent_message` items have only an `item.completed` (no started/completed
//! pairing for text). Tool-kind items (`command_execution`, `file_change`,
//! `mcp_tool_call`, `web_search`, `plan_update`) DO pair via the shared
//! [`ToolPairingBuffer`]. Multiple `item.completed` events may arrive per
//! turn — the read loop continues until `turn.completed` (success) or
//! `turn.failed` (failure) arrives.

use super::{Harness, HarnessEvent, HarnessKind, ToolPairingBuffer};
use crate::chat_adapters::Attachment;
use crate::command::{CodexCloudSubcommand, CodexSubcommand, HarnessOptions};
#[cfg(test)]
use crate::command::{CodexExtras, HarnessExtras};
use crate::config::CodexConfig;
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

/// Codex CLI-subprocess harness.
pub struct CodexHarness {
    config: CodexConfig,
    /// Map of opaque session-name → codex `thread_id`. Keys here are the raw
    /// user-supplied names; the App layer owns the prefixed `codex:{name}`
    /// form used for cross-harness isolation in state.
    sessions: Arc<StdMutex<HashMap<String, String>>>,
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
    /// Schema registry. Defaults to an empty registry; `with_schema_registry`
    /// swaps in the App-shared one. Symmetric with `ClaudeHarness` so the
    /// failure mode is consistent across harnesses: an unknown
    /// `--schema=<name>` always produces a deterministic "unknown schema /
    /// not registered" error rather than a silent no-op when the builder is
    /// forgotten. (Critic-4 P1.1.)
    schema_registry: Arc<crate::structured_output::SchemaRegistry>,
}

impl CodexHarness {
    /// Construct a harness with the provided config and ambient-event sender.
    pub fn new(config: CodexConfig, ambient_tx: broadcast::Sender<AmbientEvent>) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: Some(ambient_tx),
            schema_registry: Arc::new(crate::structured_output::SchemaRegistry::default()),
        }
    }

    /// Test constructor with no ambient-event broadcast.
    #[cfg(test)]
    pub fn new_with_config(config: CodexConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            ambient_tx: None,
            schema_registry: Arc::new(crate::structured_output::SchemaRegistry::default()),
        }
    }

    /// Builder: attach a [`SchemaRegistry`] so `--schema=<name>` can resolve
    /// against `[schemas.<name>]` config entries, with full webhook-delivery
    /// parity to the claude harness.
    pub fn with_schema_registry(
        mut self,
        registry: Arc<crate::structured_output::SchemaRegistry>,
    ) -> Self {
        self.schema_registry = registry;
        self
    }

    /// Spawn a chat-safe codex subcommand (`sessions` / `apply <id>` /
    /// `cloud <sub>`) and stream the captured stdout back as a single
    /// fenced-code Text event followed by Done. Mirrors
    /// [`OpencodeHarness::run_subcommand`] / [`GeminiHarness::run_subcommand`]:
    /// emits exactly one of `Text(fenced_stdout) + Done` or `Error(msg) + Done`.
    /// No ambient events; subcommands are not AI runs and don't carry session
    /// state. Bounded by a 30s timeout; output is truncated at 3000 chars
    /// inside the fenced block to keep chat responses bounded.
    pub async fn run_subcommand(
        &self,
        sub: CodexSubcommand,
        args: Vec<String>,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(32);
        let binary = self
            .config
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("codex"));

        let argv = build_subcommand_argv(&sub, &args);
        tokio::spawn(async move {
            let body = run_subcommand_inner(binary, argv, sub, args, event_tx.clone());
            let result = AssertUnwindSafe(body).catch_unwind().await;
            if let Err(panic_info) = result {
                let msg = format_panic_message(&*panic_info);
                let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                let _ = event_tx
                    .send(HarnessEvent::Done {
                        session_id: String::new(),
                    })
                    .await;
            }
        });
        Ok(event_rx)
    }
}

/// Pure function: build the argv for a chat-safe codex subcommand.
///
/// - `sessions` → `resume --all` (read-only listing of saved sessions)
/// - `apply` / `cloud apply` → `apply <task_id>` / `cloud apply <task_id>`
/// - `cloud {list|status|diff|exec}` → `cloud <sub> …`
fn build_subcommand_argv(sub: &CodexSubcommand, extra_args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = match sub {
        CodexSubcommand::Sessions => vec!["resume".into(), "--all".into()],
        CodexSubcommand::Apply => vec!["apply".into()],
        CodexSubcommand::Cloud(c) => match c {
            CodexCloudSubcommand::List => vec!["cloud".into(), "list".into()],
            CodexCloudSubcommand::Status => vec!["cloud".into(), "status".into()],
            CodexCloudSubcommand::Diff => vec!["cloud".into(), "diff".into()],
            CodexCloudSubcommand::Apply => vec!["cloud".into(), "apply".into()],
            CodexCloudSubcommand::Exec => vec!["cloud".into(), "exec".into()],
        },
    };
    argv.extend(extra_args.iter().cloned());
    argv
}

fn sub_display_name(sub: &CodexSubcommand) -> &'static str {
    match sub {
        CodexSubcommand::Sessions => "sessions",
        CodexSubcommand::Apply => "apply",
        CodexSubcommand::Cloud(c) => match c {
            CodexCloudSubcommand::List => "cloud list",
            CodexCloudSubcommand::Status => "cloud status",
            CodexCloudSubcommand::Diff => "cloud diff",
            CodexCloudSubcommand::Apply => "cloud apply",
            CodexCloudSubcommand::Exec => "cloud exec",
        },
    }
}

/// Async inner body: spawn child, wait for output, emit Text + Done (or
/// Error + Done). Bounded by a 30s timeout. Stdout is truncated at 3000
/// chars inside the fenced block to keep chat responses bounded. Emits
/// Done unconditionally before returning so receivers don't hang.
async fn run_subcommand_inner(
    binary: PathBuf,
    argv: Vec<String>,
    sub: CodexSubcommand,
    extra_args: Vec<String>,
    event_tx: mpsc::Sender<HarnessEvent>,
) {
    let sub_label = sub_display_name(&sub);

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
                    "codex binary not found: {} (set [harness.codex] binary_path or install codex on PATH)",
                    binary.display()
                )
            } else {
                format!("codex {} spawn failed: {}", sub_label, e)
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

    let out =
        match tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!(
                        "codex {} wait failed: {}",
                        sub_label, e
                    )))
                    .await;
                let _ = event_tx
                    .send(HarnessEvent::Done {
                        session_id: String::new(),
                    })
                    .await;
                return;
            }
            Err(_) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!(
                        "codex {} timed out after 30s",
                        sub_label
                    )))
                    .await;
                let _ = event_tx
                    .send(HarnessEvent::Done {
                        session_id: String::new(),
                    })
                    .await;
                return;
            }
        };

    if !out.status.success() {
        let raw_stderr = if out.stderr.len() > 64 * 1024 {
            &out.stderr[..64 * 1024]
        } else {
            &out.stderr
        };
        let stderr_trim = String::from_utf8_lossy(raw_stderr).trim().to_string();
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let sanitized = sanitize_stderr(&stderr_trim);
        // Log the sanitized stderr (env-vars and absolute user paths redacted)
        // rather than the raw form so `RUST_LOG=debug` captures don't leak
        // secrets into shared log aggregators. (Security review LOW-1.)
        tracing::debug!(
            "codex {} non-zero exit; sanitized stderr: {}",
            sub_label,
            sanitized
        );
        let detail = if sanitized.is_empty() {
            format!(
                "codex {} exited with status {} (no stderr)",
                sub_label, code
            )
        } else {
            format!(
                "codex {} exited with status {}: {}",
                sub_label, code, sanitized
            )
        };
        let _ = event_tx.send(HarnessEvent::Error(detail)).await;
        let _ = event_tx
            .send(HarnessEvent::Done {
                session_id: String::new(),
            })
            .await;
        return;
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stripped = stdout.trim();

    let message = if stripped.is_empty() {
        let arg_summary = if extra_args.is_empty() {
            String::new()
        } else {
            format!(" {}", extra_args.join(" "))
        };
        // "no results" matches the wording used by gemini and opencode for the
        // same situation (Code-reviewer MED-1, cross-harness wording parity).
        format!("codex {}{}: no results", sub_label, arg_summary)
    } else {
        // Truncate at 3000 chars for chat-friendliness, mirroring opencode.
        const MAX_CHARS: usize = 3000;
        let body = if stripped.chars().count() > MAX_CHARS {
            let truncated: String = stripped.chars().take(MAX_CHARS).collect();
            format!(
                "{}\n…[truncated; run from terminal for full output]",
                truncated
            )
        } else {
            stripped.to_string()
        };
        format!("```\n{}\n```", body)
    };

    let _ = event_tx.send(HarnessEvent::Text(message)).await;
    let _ = event_tx
        .send(HarnessEvent::Done {
            session_id: String::new(),
        })
        .await;
}

#[async_trait]
impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
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

        // Validate attachment MIME types (codex accepts only images via -i).
        // Attachments are already on-disk temp files at `att.path`; we just
        // pass the paths through. The owning `Attachment` struct deletes the
        // temp file in its Drop, but we ALSO clean up explicitly after the
        // subprocess to mirror opencode's pattern (defense-in-depth).
        if let Err(msg) = validate_attachments(attachments) {
            let _ = event_tx.send(HarnessEvent::Error(msg)).await;
            let _ = event_tx
                .send(HarnessEvent::Done {
                    session_id: String::new(),
                })
                .await;
            return Ok(event_rx);
        }
        let attachment_paths: Vec<PathBuf> = attachments.iter().map(|a| a.path.clone()).collect();

        // Schema temp file (only when --schema is set).
        let resolved = match handle_schema(
            options.schema.as_deref(),
            cwd,
            Some(self.schema_registry.as_ref()),
        ) {
            Ok(r) => r,
            Err(msg) => {
                let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                let _ = event_tx
                    .send(HarnessEvent::Done {
                        session_id: String::new(),
                    })
                    .await;
                return Ok(event_rx);
            }
        };
        let schema_temp = resolved.temp;
        let registered_schema_name = resolved.registered_name;

        let binary: PathBuf = self
            .config
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("codex"));

        let attachment_path_refs: Vec<&Path> =
            attachment_paths.iter().map(|p| p.as_path()).collect();
        let schema_path: Option<&Path> = schema_temp.as_ref().map(|t| t.path());

        let args = build_argv(
            prompt,
            session_id,
            options,
            &self.config,
            &attachment_path_refs,
            schema_path,
        );

        let cwd = cwd.to_path_buf();
        let ambient_tx = self.ambient_tx.clone();
        let run_id = ulid::Ulid::new().to_string();

        if let Some(ref tx) = ambient_tx {
            let _ = tx.send(AmbientEvent::HarnessStarted {
                harness: HarnessKind::Codex.name().to_string(),
                run_id: run_id.clone(),
                prompt_hash: None,
            });
        }

        // Decide whether this run should emit `HarnessEvent::StructuredOutput`
        // (parity with claude) or fall back to plain `HarnessEvent::Text`.
        let structured_schema_for_run: Option<String> = decide_structured_schema(
            registered_schema_name.as_deref(),
            Some(self.schema_registry.as_ref()),
        );

        let run_id_for_task = run_id.clone();
        // Move the schema temp + attachment paths into the task so they
        // outlive the subprocess. The schema TempFile drop-cleans the tmp
        // file. Attachment paths are explicitly removed after the run.
        tokio::spawn(async move {
            let _schema_guard = schema_temp;

            let body = run_codex_inner(
                binary,
                args,
                cwd,
                event_tx.clone(),
                structured_schema_for_run,
            );
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> =
                AssertUnwindSafe(body).catch_unwind().await;

            // Clean up attachment temp files. The Attachment::Drop is a
            // fallback — explicit cleanup here mirrors opencode's pattern.
            for path in &attachment_paths {
                if let Err(e) = std::fs::remove_file(path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(
                            "codex: failed to remove attachment temp {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }

            let status = match result {
                Ok(()) => "ok",
                Err(panic_info) => {
                    let msg = format_panic_message(&*panic_info);
                    let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                    // The panic short-circuited `run_codex_inner` before its
                    // own `Done` emission. Send one here so the receiver
                    // doesn't block forever waiting for a terminal event.
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
                    harness: HarnessKind::Codex.name().to_string(),
                    run_id: run_id_for_task,
                    status: status.to_string(),
                });
            }
        });

        Ok(event_rx)
    }

    fn get_session_id(&self, session_name: &str) -> Option<String> {
        // Recover from poison rather than panicking. A panic in another
        // session-mutating call shouldn't make subsequent reads also panic.
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(session_name).cloned()
    }

    fn set_session_id(&self, session_name: &str, session_id: String) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.insert(session_name.to_string(), session_id);
    }
}

// `Arc<CodexHarness>` is a `Harness` via the blanket `impl<H> Harness for
// `Arc<H>` in `harness::mod`. No per-type forwarder needed.

/// Build the argv for `codex exec [...] --json [...]`. Pure function —
/// `session_id` is the *resolved* codex thread_id (or `None` for fresh).
///
/// Branches:
/// - Fresh prompt: `codex exec --json -s workspace-write --skip-git-repo-check [...] -- <prompt>`
/// - Resume by id: `codex exec resume --json -s workspace-write --skip-git-repo-check [...] <session_id> <prompt>`
/// - Bare --continue: `codex exec resume --json [...] --last <prompt>`
///
/// **Always-on flags** (cannot be overridden by user options):
/// - `-s <sandbox>` — defaults to `workspace-write` when no user/config value
///   is set. Replaces the deprecated-in-0.128 `--full-auto` flag (`codex exec`
///   no longer accepts `--ask-for-approval` and runs non-interactively by
///   default; the explicit sandbox value avoids the deprecation warning).
/// - `--skip-git-repo-check` (terminus's tmux cwd may not be a git repo).
/// - `--ignore-user-config` if `config.ignore_user_config = true`.
/// - `--ephemeral` when no named/resumed session is active (one-shot prompts).
fn build_argv(
    prompt: &str,
    session_id: Option<&str>,
    options: &HarnessOptions,
    config: &CodexConfig,
    attachment_paths: &[&Path],
    schema_path: Option<&Path>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];

    // Resume mode is a SUBCOMMAND, not a flag.
    let is_resume = session_id.is_some() || options.continue_last;
    if is_resume {
        args.push("resume".into());
    }

    args.push("--json".into());
    args.push("--skip-git-repo-check".into());

    if config.ignore_user_config {
        args.push("--ignore-user-config".into());
    }

    // Sandbox: per-prompt > config > workspace-write fallback.
    //
    // Codex 0.128 deprecated `--full-auto` in favor of explicit
    // `-s <sandbox>`. `codex exec` runs non-interactively by default in 0.128
    // (no `--ask-for-approval` flag exists on the exec subcommand), so we just
    // need to pin the sandbox. `workspace-write` matches the deprecated
    // `--full-auto` behavior — write access to the current workspace, no
    // network, no parent-dir reads.
    //
    // Config-sourced values bypass the parser, so validate here too: drop +
    // log on invalid input rather than emitting a malformed flag.
    const VALID_SANDBOX: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
    let codex_e = options.codex_extras();
    let effective_sandbox = codex_e
        .and_then(|c| c.sandbox.as_ref())
        .or(config.sandbox.as_ref());
    let sandbox_value: &str = match effective_sandbox {
        Some(s) if VALID_SANDBOX.contains(&s.as_str()) => s.as_str(),
        Some(s) => {
            tracing::warn!(
                value = %s,
                "codex: invalid sandbox value in config (valid: read-only, workspace-write, danger-full-access); falling back to workspace-write"
            );
            "workspace-write"
        }
        None => "workspace-write",
    };
    args.push("-s".into());
    args.push(sandbox_value.into());

    // Model: per-prompt > config > codex default.
    let effective_model = options.model.as_ref().or(config.model.as_ref());
    if let Some(m) = effective_model {
        args.push("-m".into());
        args.push(m.clone());
    }

    // Profile: per-prompt > config > codex default.
    let effective_profile = codex_e
        .and_then(|c| c.profile.as_ref())
        .or(config.profile.as_ref());
    if let Some(p) = effective_profile {
        args.push("-p".into());
        args.push(p.clone());
    }

    // Additional writable directories, repeatable. `codex exec --add-dir <DIR>`
    // extends the sandbox beyond the current cwd (verified through 0.128).
    // Sourced from `HarnessOptions.add_dirs` (the same field claude consumes
    // via `--add-dir`); per-prompt only — no codex-config equivalent yet.
    for dir in &options.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.display().to_string());
    }

    // Image attachments (repeatable).
    for path in attachment_paths {
        args.push("-i".into());
        args.push(path.display().to_string());
    }

    // Output schema.
    if let Some(p) = schema_path {
        args.push("--output-schema".into());
        args.push(p.display().to_string());
    }

    // Ephemeral: only when there's no named or resumed session.
    let has_named_session = options.name.is_some();
    if !is_resume && !has_named_session {
        args.push("--ephemeral".into());
    }

    // Resume positional: <SESSION_ID> or --last.
    // Defensive: a stored session_id can be empty (corrupted state, programmer
    // error) which would produce malformed argv `codex exec resume … "" <prompt>`.
    // Treat empty as `--last` so we degrade gracefully instead of failing the
    // run with a parse error from codex.
    if is_resume {
        let sid = session_id.filter(|s| !s.is_empty());
        if let Some(s) = sid {
            args.push(s.to_string());
        } else {
            args.push("--last".into());
        }
    }

    // Prompt as the final positional.
    if !prompt.is_empty() {
        args.push(prompt.to_string());
    }

    args
}

/// Validate attachment MIME types — codex's `-i` flag accepts only images.
/// Non-image attachments produce an `Err` with a chat-safe message.
///
/// The attachments are already on-disk temp files (managed by the chat
/// adapter). Codex receives the paths via `-i`; the harness explicitly
/// cleans them up after the subprocess exits.
fn validate_attachments(attachments: &[Attachment]) -> std::result::Result<(), String> {
    const ALLOWED_MIME: &[&str] = &["image/png", "image/jpeg", "image/jpg", "image/webp"];
    for att in attachments {
        // Case-insensitive comparison: some chat platforms send `Image/PNG`
        // (capitalized) per legacy convention. RFC 2045 says MIME types are
        // case-insensitive in their type/subtype components.
        let mime_lower = att.media_type.to_ascii_lowercase();
        if !ALLOWED_MIME.contains(&mime_lower.as_str()) {
            return Err(format!(
                "codex: only image attachments supported (got {} — allowed: {})",
                att.media_type,
                ALLOWED_MIME.join(", ")
            ));
        }
    }
    Ok(())
}

/// Resolved `--schema` source. The harness keeps the temp file alive across
/// the run; the optional `registered_name` lets the spawned task know it
/// should consider webhook delivery on success.
#[derive(Debug)]
struct ResolvedSchema {
    temp: Option<TempFile>,
    /// `Some(name)` when `--schema=<name>` matched a `[schemas.<name>]` entry
    /// from `terminus.toml`. `None` for inline-JSON / file-path forms.
    registered_name: Option<String>,
}

/// Decide whether a run should emit `HarnessEvent::StructuredOutput`
/// (which feeds the webhook-delivery pipeline) or fall back to plain
/// `HarnessEvent::Text`.
///
/// The structured-output path is taken iff:
/// 1. `--schema=<registered-name>` matched a `[schemas.<name>]` entry, AND
/// 2. that entry has a `webhook` URL configured.
///
/// Registry-driven so the security model (HMAC secret env var) is honoured —
/// inline-JSON and file-path schema forms never trigger webhook delivery.
fn decide_structured_schema(
    registered_name: Option<&str>,
    registry: Option<&crate::structured_output::SchemaRegistry>,
) -> Option<String> {
    let name = registered_name?;
    let reg = registry?;
    reg.webhook_for(name).map(|_| name.to_string())
}

/// Resolve the `--schema` option in priority order:
/// 1. **Registered name** (`[schemas.<name>]` in `terminus.toml`) — only when
///    a [`SchemaRegistry`] is attached. Highest priority because the security
///    model (HMAC secret env var) is registry-driven.
/// 2. **File path** — relative to `cwd` or absolute. Validated as UTF-8 +
///    JSON before being copied to a temp.
/// 3. **Inline JSON** — parsed and written to a temp.
///
/// Returns `Ok(ResolvedSchema { temp: None, registered_name: None })` when no
/// schema was requested. When the user provides a path that exists, codex
/// needs the path on disk; we copy to a temp so the harness has a uniform
/// `guard.path()` surface for argv construction.
fn handle_schema(
    schema: Option<&str>,
    cwd: &Path,
    registry: Option<&crate::structured_output::SchemaRegistry>,
) -> std::result::Result<ResolvedSchema, String> {
    let Some(s) = schema else {
        return Ok(ResolvedSchema {
            temp: None,
            registered_name: None,
        });
    };
    let s = s.trim();
    if s.is_empty() {
        return Err("codex: --schema value is empty".into());
    }

    // (1) Registered-name lookup — beats path / inline-JSON forms.
    if let Some(reg) = registry {
        if let Some(value) = reg.schema_value(s) {
            let bytes = match serde_json::to_vec_pretty(value) {
                Ok(b) => b,
                Err(e) => {
                    return Err(format!(
                        "codex: failed to serialize registered schema '{}': {}",
                        s, e
                    ));
                }
            };
            let temp = write_schema_temp(&bytes)?;
            return Ok(ResolvedSchema {
                temp,
                registered_name: Some(s.to_string()),
            });
        }
    }

    // (2) File path.
    let candidate = if Path::new(s).is_absolute() {
        PathBuf::from(s)
    } else {
        cwd.join(s)
    };
    if candidate.is_file() {
        let bytes = match std::fs::read(&candidate) {
            Ok(b) => b,
            Err(e) => return Err(format!("codex: failed to read schema file: {}", e)),
        };
        // Validate the file is valid UTF-8 + JSON before passing to codex.
        // Without this, ANY readable file (e.g. a binary or /etc/passwd) is
        // silently copied to a temp and handed to codex — defending against
        // an unintended file-read code path.
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => return Err("codex: schema file is not valid UTF-8".into()),
        };
        if let Err(e) = serde_json::from_str::<serde_json::Value>(text) {
            return Err(format!("codex: schema file is not valid JSON: {}", e));
        }
        return Ok(ResolvedSchema {
            temp: write_schema_temp(&bytes)?,
            registered_name: None,
        });
    }
    // Explicit error for the "exists but not a file" case (typically a
    // directory). The previous fallthrough produced a misleading
    // "neither a file path nor valid JSON" error for valid-looking paths.
    //
    // Echoing `candidate.display()` here would reflect the cwd-joined
    // resolved path back to chat — a small information-disclosure oracle
    // for whoever sent the prompt (Critic-6 P1.2). Echo only the user's
    // raw input instead.
    if candidate.exists() {
        return Err(format!(
            "codex: --schema path exists but is not a file (directory or special file): {}",
            s
        ));
    }

    // (3) Inline JSON.
    if let Err(e) = serde_json::from_str::<serde_json::Value>(s) {
        // If a registry is attached, fold the known-schemas list into the
        // error so users discover the registered-name shortcut. Cap at 5
        // names with an "…and N more" suffix so a 50-schema registry
        // doesn't produce an unreadable 500-character error line.
        // (Critic-4 P2.5.)
        let known_hint = match registry {
            Some(reg) if !reg.schema_names().is_empty() => {
                let names = reg.schema_names();
                const SHOW: usize = 5;
                if names.len() <= SHOW {
                    format!(" (registered schemas: {})", names.join(", "))
                } else {
                    format!(
                        " (registered schemas: {}, …and {} more)",
                        names[..SHOW].join(", "),
                        names.len() - SHOW
                    )
                }
            }
            _ => String::new(),
        };
        return Err(format!(
            "codex: --schema is neither a file path nor valid JSON{}: {}",
            known_hint, e
        ));
    }
    Ok(ResolvedSchema {
        temp: write_schema_temp(s.as_bytes())?,
        registered_name: None,
    })
}

fn write_schema_temp(bytes: &[u8]) -> std::result::Result<Option<TempFile>, String> {
    let mut tmp = match tempfile::Builder::new()
        .prefix("terminus-codex-schema-")
        .suffix(".json")
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => return Err(format!("codex: failed to allocate schema temp: {}", e)),
    };
    use std::io::Write;
    if let Err(e) = tmp.write_all(bytes) {
        return Err(format!("codex: failed to write schema temp: {}", e));
    }
    if let Err(e) = tmp.flush() {
        return Err(format!("codex: failed to flush schema temp: {}", e));
    }
    Ok(Some(TempFile(tmp)))
}

/// Wrapper around `tempfile::NamedTempFile` so the harness can hold and pass
/// `&Path` references without exposing the inner type publicly.
#[derive(Debug)]
struct TempFile(tempfile::NamedTempFile);

impl TempFile {
    fn path(&self) -> &Path {
        self.0.path()
    }
}

/// Sanitize codex stderr before forwarding it to chat. Strips known-benign
/// log lines that the codex CLI emits during normal operation, then defers to
/// the shared base in [`crate::harness::sanitize_stderr_base`] for env-var
/// and path redaction plus 500-char truncation.
pub(crate) fn sanitize_stderr(s: &str) -> String {
    let filtered: String = s
        .lines()
        .filter(|line| {
            !line.contains("failed to record rollout items: thread")
                && !line.contains("Reading additional input from stdin")
        })
        .collect::<Vec<_>>()
        .join("\n");
    crate::harness::sanitize_stderr_base(&filtered)
}

fn format_panic_message(info: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = info.downcast_ref::<&str>() {
        format!("codex: internal panic: {}", s)
    } else if let Some(s) = info.downcast_ref::<String>() {
        format!("codex: internal panic: {}", s)
    } else {
        "codex: internal panic (unknown)".to_string()
    }
}

/// Spawn the codex child, read NDJSON events, translate them, and drive a
/// [`ToolPairingBuffer`] so emitted `HarnessEvent::ToolUse` entries carry
/// both `input` (from `item.started`) and `output` (from `item.completed`).
///
/// When `structured_schema` is `Some(name)`, agent_message text is BUFFERED
/// (not emitted as `HarnessEvent::Text`) and on the terminal `turn.completed`
/// the buffer is parsed as JSON and emitted as `HarnessEvent::StructuredOutput
/// { schema: name, value, run_id }`. This is the path that drives webhook
/// delivery via `drive_harness`. When `None`, agent_message text streams as
/// `HarnessEvent::Text` exactly as before (existing behavior preserved).
async fn run_codex_inner(
    binary: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    event_tx: mpsc::Sender<HarnessEvent>,
    structured_schema: Option<String>,
) {
    let mut cmd = Command::new(&binary);
    cmd.args(&args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // CRITICAL: `codex exec` reads stdin even when prompt is supplied as
        // a positional arg (still true in 0.128 — codex prints "Reading
        // additional input from stdin..." before continuing). Pin stdin to
        // /dev/null or it blocks indefinitely.
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "codex: binary not found at {} (set [harness.codex] binary_path or install codex on PATH; e.g. `brew install --cask codex` or `npm install -g @openai/codex`)",
                    binary.display()
                )
            } else {
                format!("codex: spawn failed: {}", e)
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
                    "codex: stdout pipe missing".to_string(),
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

    // Drain stderr concurrently with stdout so codex doesn't pipe-deadlock
    // when its stderr buffer fills (~64 KiB on Linux, ~8 KiB on macOS).
    //
    // The buffered portion (first 64 KiB) is captured for sanitization on
    // non-zero exit. Anything past 64 KiB is forwarded to `tokio::io::sink`
    // so the child can keep writing without blocking — without the post-cap
    // drain, codex could fill the OS pipe buffer past 64 KiB and stall on
    // its next stderr write, hanging `child.wait()` until the 5-minute
    // idle timeout fired. (Critic-2 P1.)
    let stderr_handle: Option<tokio::task::JoinHandle<Vec<u8>>> = child.stderr.take().map(|s| {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf: Vec<u8> = Vec::new();
            let mut s = s;
            {
                let mut limited = (&mut s).take(64 * 1024);
                let _ = limited.read_to_end(&mut buf).await;
            }
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut s, &mut sink).await;
            let _ = sink.flush().await;
            buf
        })
    });

    let mut reader = BufReader::new(stdout).lines();
    let mut captured_thread_id: Option<String> = None;
    let mut saw_recognized = false;
    let mut saw_unknown = false;
    let mut first_unknown_logged = false;
    let mut fatal_error: Option<String> = None;
    let mut pairing = ToolPairingBuffer::new();
    // Accumulator for the structured-output path. When `structured_schema` is
    // `Some(_)`, agent_message text is appended here instead of streamed as
    // `HarnessEvent::Text`; on `turn.completed` the buffer is parsed as JSON
    // and emitted via `HarnessEvent::StructuredOutput`.
    let mut structured_buffer: String = String::new();

    loop {
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(300), reader.next_line()).await;

        let line = match next {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!("codex: stdout read: {}", e)))
                    .await;
                break;
            }
            Err(_) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(
                        "codex: no output for 5 minutes — killing subprocess".to_string(),
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
                tracing::debug!("codex: failed to parse JSON line: {} ({})", e, trimmed);
                continue;
            }
        };

        match translate_event(&value) {
            Some(events) => {
                let mut terminate = false;
                for ev in events {
                    match ev {
                        TranslatedEvent::ThreadStarted { thread_id } => {
                            saw_recognized = true;
                            if !thread_id.is_empty() {
                                captured_thread_id = Some(thread_id);
                            }
                        }
                        TranslatedEvent::TurnStarted => {
                            saw_recognized = true;
                        }
                        TranslatedEvent::AgentMessage(text) => {
                            saw_recognized = true;
                            if !text.is_empty() {
                                if structured_schema.is_some() {
                                    // Buffer for parse-and-emit on
                                    // turn.completed. Concatenate without
                                    // separators — codex's --output-schema
                                    // produces a single JSON document.
                                    structured_buffer.push_str(&text);
                                } else {
                                    let _ = event_tx.send(HarnessEvent::Text(text)).await;
                                }
                            }
                        }
                        TranslatedEvent::ToolUseStart {
                            item_id,
                            tool,
                            input,
                        } => {
                            saw_recognized = true;
                            pairing.on_use(item_id, tool, input);
                        }
                        TranslatedEvent::ToolResult {
                            item_id,
                            success,
                            output,
                            error,
                        } => {
                            saw_recognized = true;
                            if let Some(paired) =
                                pairing.on_result(&item_id, success, output, error)
                            {
                                let _ = event_tx.send(paired).await;
                            } else {
                                tracing::debug!(
                                    "codex: item.completed for unknown id '{}' — ignoring",
                                    item_id
                                );
                            }
                        }
                        TranslatedEvent::NonFatalError(msg) => {
                            saw_recognized = true;
                            let _ = event_tx
                                .send(HarnessEvent::Error(format!("codex: {}", msg)))
                                .await;
                        }
                        TranslatedEvent::TurnCompleted => {
                            saw_recognized = true;
                            // Structured-output emission, if enabled.
                            if let Some(ref schema_name) = structured_schema {
                                if structured_buffer.is_empty() {
                                    // Empty buffer in structured mode means
                                    // codex completed with no agent_message
                                    // (model produced reasoning only, or
                                    // a tool-only turn). The bare
                                    // `!saw_recognized` fallback further
                                    // down doesn't fire here because
                                    // saw_recognized is true, so we'd
                                    // surface a misleading "no response
                                    // received" from the App layer
                                    // (Critic-1 P1.3). Emit an explicit
                                    // chat message instead so the user
                                    // knows the run completed but no
                                    // structured payload was produced.
                                    let _ = event_tx
                                        .send(HarnessEvent::Text(format!(
                                            "codex: --schema='{}' completed without producing structured output (no agent_message in this turn)",
                                            schema_name
                                        )))
                                        .await;
                                } else {
                                    match serde_json::from_str::<serde_json::Value>(
                                        &structured_buffer,
                                    ) {
                                        Ok(value) => {
                                            let run_id = ulid::Ulid::new().to_string();
                                            let _ = event_tx
                                                .send(HarnessEvent::StructuredOutput {
                                                    schema: schema_name.clone(),
                                                    value,
                                                    run_id,
                                                })
                                                .await;
                                        }
                                        Err(e) => {
                                            // Schema-constrained mode but
                                            // the model returned non-JSON.
                                            // `drive_harness` breaks on
                                            // the first Error event, so
                                            // any subsequent Text would be
                                            // dropped — emit only the
                                            // Error with an action hint
                                            // pointing at the codex prompt
                                            // / model choice as the likely
                                            // root cause. (Critic-1 P1.1,
                                            // Critic-2 P1.2, Critic-4 P1.3.)
                                            let _ = event_tx
                                                .send(HarnessEvent::Error(format!(
                                                    "codex: --schema='{}' but response was not valid JSON ({}). \
                                                     The model produced free-form text instead of schema-conformant JSON; \
                                                     try a clearer prompt or a model with stronger structured-output support.",
                                                    schema_name, e
                                                )))
                                                .await;
                                        }
                                    }
                                }
                            }
                            terminate = true;
                            break;
                        }
                        TranslatedEvent::TurnFailed(msg) => {
                            saw_recognized = true;
                            fatal_error = Some(msg);
                            terminate = true;
                            break;
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
                        "codex emitted an unrecognized event type; version drift possible"
                    );
                    first_unknown_logged = true;
                } else {
                    tracing::debug!("codex: unknown event type `{}`", ty);
                }
            }
        }
    }

    for ev in pairing.flush_pending() {
        let _ = event_tx.send(ev).await;
    }

    let status = match child.wait().await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("codex: child.wait() failed: {}", e);
            None
        }
    };

    let had_fatal = fatal_error.is_some();
    if let Some(msg) = fatal_error.take() {
        let _ = event_tx
            .send(HarnessEvent::Error(format!("codex: {}", msg)))
            .await;
    }

    // Always await the stderr-drain handle (even on success paths) so the
    // spawned task ends cleanly. The buffered stderr is only used on
    // non-zero-exit paths but the drain task must run to completion to
    // avoid a leaked tokio task and a half-open pipe.
    let raw_stderr_buf: Vec<u8> = match stderr_handle {
        Some(h) => h.await.unwrap_or_else(|e| {
            tracing::warn!("codex stderr drain task failed: {}", e);
            Vec::new()
        }),
        None => Vec::new(),
    };

    if let Some(status) = status {
        if !status.success() && !had_fatal {
            let stderr_raw = String::from_utf8_lossy(&raw_stderr_buf).trim().to_string();
            let sanitized = sanitize_stderr(&stderr_raw);
            tracing::debug!("codex run non-zero exit; stderr: {}", sanitized);
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let detail = if sanitized.is_empty() {
                format!("codex exited with status {} (no stderr)", code)
            } else {
                format!("codex exited with status {}: {}", code, sanitized)
            };
            let _ = event_tx.send(HarnessEvent::Error(detail)).await;
        }
    }

    if !saw_recognized {
        let msg = if saw_unknown {
            "codex: no recognized events received (version drift — check `codex --version`)"
                .to_string()
        } else if !had_fatal {
            "codex: no response content".to_string()
        } else {
            String::new()
        };
        if !msg.is_empty() {
            let _ = event_tx.send(HarnessEvent::Text(msg)).await;
        }
    }

    let sid = captured_thread_id.clone().unwrap_or_default();
    let _ = event_tx.send(HarnessEvent::Done { session_id: sid }).await;
}

/// Pure translation of a single codex JSON event into zero-or-more
/// [`TranslatedEvent`]s. Returns `None` for unrecognized event types.
pub(crate) fn translate_event(event: &serde_json::Value) -> Option<Vec<TranslatedEvent>> {
    let ty = event.get("type").and_then(|t| t.as_str())?;
    match ty {
        "thread.started" => {
            let thread_id = event
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(vec![TranslatedEvent::ThreadStarted { thread_id }])
        }
        "turn.started" => Some(vec![TranslatedEvent::TurnStarted]),
        "item.started" => translate_item_started(event),
        "item.completed" => translate_item_completed(event),
        "turn.completed" => Some(vec![TranslatedEvent::TurnCompleted]),
        "turn.failed" => {
            let msg = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "codex turn failed".to_string());
            Some(vec![TranslatedEvent::TurnFailed(msg)])
        }
        "error" => {
            let msg = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("codex error")
                .to_string();
            Some(vec![TranslatedEvent::NonFatalError(msg)])
        }
        _ => None,
    }
}

fn translate_item_started(event: &serde_json::Value) -> Option<Vec<TranslatedEvent>> {
    let item = event.get("item")?;
    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() || item_type.is_empty() {
        return Some(Vec::new());
    }
    if !is_tool_kind(item_type) {
        return Some(Vec::new());
    }
    let input = describe_item_input(item_type, item);
    Some(vec![TranslatedEvent::ToolUseStart {
        item_id: id.to_string(),
        tool: item_type.to_string(),
        input,
    }])
}

fn translate_item_completed(event: &serde_json::Value) -> Option<Vec<TranslatedEvent>> {
    let item = event.get("item")?;
    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if item_type == "agent_message" {
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Some(vec![TranslatedEvent::AgentMessage(text)]);
    }
    if item_type == "reasoning" {
        return Some(Vec::new());
    }
    if !is_tool_kind(item_type) || id.is_empty() {
        return Some(Vec::new());
    }

    let output = item
        .get("aggregated_output")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let exit_code = item.get("exit_code").and_then(|v| v.as_i64());

    // Success discriminator differs by tool kind. `command_execution` is the
    // only kind that carries a meaningful `exit_code`; for the others
    // (`file_change`, `mcp_tool_call`, `web_search`, `plan_update`) the
    // canonical "completed without error" signal is `status == "completed"`,
    // which is also how codex marks `command_execution` success at the
    // status layer. We trust `exit_code` for `command_execution` (the
    // historical contract) and fall back to `status` for everything else
    // so a future codex schema that adds a non-zero exit_code or a
    // `status: "failed"` to e.g. `file_change` is correctly classified.
    let status_completed = item.get("status").and_then(|v| v.as_str()) == Some("completed");
    let success = if item_type == "command_execution" {
        exit_code == Some(0)
    } else {
        status_completed
    };
    let error = if success {
        None
    } else if item_type == "command_execution" {
        Some(format!(
            "exit_code={}",
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into())
        ))
    } else {
        Some(format!(
            "status={}",
            item.get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        ))
    };

    Some(vec![TranslatedEvent::ToolResult {
        item_id: id.to_string(),
        success,
        output,
        error,
    }])
}

fn is_tool_kind(item_type: &str) -> bool {
    matches!(
        item_type,
        "command_execution" | "file_change" | "mcp_tool_call" | "web_search" | "plan_update"
    )
}

fn describe_item_input(item_type: &str, item: &serde_json::Value) -> Option<String> {
    match item_type {
        "command_execution" => item
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "file_change" => item
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "web_search" => item
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => Some(serde_json::to_string(item).unwrap_or_default()),
    }
}

/// Pure-data intermediate representation emitted by [`translate_event`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TranslatedEvent {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted,
    AgentMessage(String),
    ToolUseStart {
        item_id: String,
        tool: String,
        input: Option<String>,
    },
    ToolResult {
        item_id: String,
        success: bool,
        output: Option<String>,
        error: Option<String>,
    },
    /// Non-fatal `error` event. Codex continues processing after these.
    NonFatalError(String),
    /// Terminal success.
    TurnCompleted,
    /// Terminal failure with extracted error.message.
    TurnFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_harness() -> CodexHarness {
        CodexHarness::new_with_config(CodexConfig::default())
    }

    // ── kind / supports_resume ──────────────────────────────────────────────

    #[test]
    fn kind_returns_codex() {
        let h = empty_harness();
        assert_eq!(h.kind(), HarnessKind::Codex);
    }

    #[test]
    fn supports_resume_true() {
        let h = empty_harness();
        assert!(h.supports_resume());
    }

    /// Critic-3 P1.6: gemini and opencode have `arc_forwarder_*` tests that
    /// pin the `Arc<H>` blanket impl after F8 deleted the per-type
    /// forwarders. Codex was missing the same coverage — an
    /// `unimplemented!()` mutation in the blanket would survive without a
    /// codex-specific test exercising the Arc path.
    #[test]
    fn arc_forwarder_delegates_kind_and_resume_for_codex() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        assert_eq!(dyn_handle.kind(), HarnessKind::Codex);
        assert!(dyn_handle.supports_resume());
    }

    #[test]
    fn arc_forwarder_delegates_session_id_mutation_for_codex() {
        let inner = Arc::new(empty_harness());
        let dyn_handle: &dyn Harness = &inner;
        dyn_handle.set_session_id("foo", "ses_codex".into());
        assert_eq!(dyn_handle.get_session_id("foo"), Some("ses_codex".into()));
        // The inner struct sees the mutation too.
        assert_eq!(inner.get_session_id("foo"), Some("ses_codex".into()));
    }

    // ── session id storage ──────────────────────────────────────────────────

    #[test]
    fn set_and_get_session_id_roundtrip() {
        let h = empty_harness();
        h.set_session_id("auth", "thread-uuid-1".into());
        assert_eq!(h.get_session_id("auth").as_deref(), Some("thread-uuid-1"));
        assert!(h.get_session_id("nope").is_none());
    }

    // ── build_argv ──────────────────────────────────────────────────────────

    #[test]
    fn argv_fresh_prompt_passes_workspace_write_and_skip_git_repo_check() {
        // Codex 0.128 deprecated `--full-auto` in favor of `-s <sandbox>`. With
        // no per-prompt or config sandbox set, terminus pins
        // `-s workspace-write` (the same effective behavior as the deprecated
        // `--full-auto`) to avoid the deprecation warning on stderr.
        let opts = HarnessOptions::default();
        let cfg = CodexConfig::default();
        let args = build_argv("hello", None, &opts, &cfg, &[], None);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(
            !args.contains(&"--full-auto".to_string()),
            "--full-auto is deprecated; expected -s workspace-write instead. argv: {:?}",
            args
        );
        let s_pos = args
            .iter()
            .position(|a| a == "-s")
            .expect("expected -s flag in argv");
        assert_eq!(
            args.get(s_pos + 1).map(String::as_str),
            Some("workspace-write")
        );
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
        assert!(args.contains(&"--ephemeral".to_string()));
        assert_eq!(args.last().unwrap(), "hello");
    }

    #[test]
    fn argv_named_session_does_not_pass_ephemeral() {
        let opts = HarnessOptions {
            name: Some("auth".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &CodexConfig::default(), &[], None);
        assert!(
            !args.contains(&"--ephemeral".to_string()),
            "named sessions must NOT pass --ephemeral; got {:?}",
            args
        );
    }

    #[test]
    fn argv_resume_with_session_id_uses_resume_subcommand() {
        let args = build_argv(
            "keep going",
            Some("019dcf4d-aaaa-7777-bbbb-cccccccccccc"),
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            None,
        );
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "resume");
        assert!(args.contains(&"019dcf4d-aaaa-7777-bbbb-cccccccccccc".to_string()));
        assert_eq!(args.last().unwrap(), "keep going");
        assert!(
            !args.contains(&"--ephemeral".to_string()),
            "resume must not pass --ephemeral"
        );
    }

    #[test]
    fn argv_continue_last_uses_dash_dash_last() {
        let opts = HarnessOptions {
            continue_last: true,
            ..Default::default()
        };
        let args = build_argv("more", None, &opts, &CodexConfig::default(), &[], None);
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "resume");
        assert!(args.contains(&"--last".to_string()));
        assert_eq!(args.last().unwrap(), "more");
    }

    #[test]
    fn argv_sandbox_flag_when_set() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("read-only".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &CodexConfig::default(), &[], None);
        let pos = args.iter().position(|a| a == "-s").expect("-s not found");
        assert_eq!(args[pos + 1], "read-only");
    }

    #[test]
    fn argv_sandbox_per_prompt_overrides_config() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("danger-full-access".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let cfg = CodexConfig {
            sandbox: Some("read-only".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &cfg, &[], None);
        let pos = args.iter().position(|a| a == "-s").expect("-s not found");
        assert_eq!(args[pos + 1], "danger-full-access");
    }

    #[test]
    fn argv_model_flag_when_set() {
        let opts = HarnessOptions {
            model: Some("gpt-5.4".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &CodexConfig::default(), &[], None);
        let pos = args.iter().position(|a| a == "-m").expect("-m not found");
        assert_eq!(args[pos + 1], "gpt-5.4");
    }

    #[test]
    fn argv_profile_flag_when_set() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                profile: Some("dev".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &CodexConfig::default(), &[], None);
        let pos = args.iter().position(|a| a == "-p").expect("-p not found");
        assert_eq!(args[pos + 1], "dev");
    }

    #[test]
    fn argv_ignore_user_config_when_set_in_config() {
        let cfg = CodexConfig {
            ignore_user_config: true,
            ..Default::default()
        };
        let args = build_argv("hi", None, &HarnessOptions::default(), &cfg, &[], None);
        assert!(args.contains(&"--ignore-user-config".to_string()));
    }

    #[test]
    fn argv_attachments_appended_as_image_flags() {
        let p1 = PathBuf::from("/tmp/a.png");
        let p2 = PathBuf::from("/tmp/b.jpg");
        let paths: Vec<&Path> = vec![p1.as_path(), p2.as_path()];
        let args = build_argv(
            "describe",
            None,
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &paths,
            None,
        );
        let i_count = args.iter().filter(|a| *a == "-i").count();
        assert_eq!(i_count, 2);
        assert!(args.contains(&"/tmp/a.png".to_string()));
        assert!(args.contains(&"/tmp/b.jpg".to_string()));
    }

    #[test]
    fn argv_add_dir_flag_repeats_per_directory() {
        let opts = HarnessOptions {
            add_dirs: vec![
                PathBuf::from("/tmp/lib"),
                PathBuf::from("/tmp/shared"),
                PathBuf::from("/tmp/extra"),
            ],
            ..Default::default()
        };
        let args = build_argv("review", None, &opts, &CodexConfig::default(), &[], None);
        // One `--add-dir` per directory, in order.
        let positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| (a == "--add-dir").then_some(i))
            .collect();
        assert_eq!(
            positions.len(),
            3,
            "expected 3 --add-dir flags, got {:?}",
            args
        );
        assert_eq!(args[positions[0] + 1], "/tmp/lib");
        assert_eq!(args[positions[1] + 1], "/tmp/shared");
        assert_eq!(args[positions[2] + 1], "/tmp/extra");
    }

    #[test]
    fn argv_add_dir_absent_when_options_empty() {
        let args = build_argv(
            "hi",
            None,
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            None,
        );
        assert!(
            !args.iter().any(|a| a == "--add-dir"),
            "no --add-dir expected when add_dirs is empty, got {:?}",
            args
        );
    }

    #[test]
    fn argv_schema_path_passed_via_output_schema() {
        let p = PathBuf::from("/tmp/schema.json");
        let args = build_argv(
            "summarize",
            None,
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            Some(p.as_path()),
        );
        let pos = args
            .iter()
            .position(|a| a == "--output-schema")
            .expect("--output-schema not found");
        assert_eq!(args[pos + 1], "/tmp/schema.json");
    }

    // ── handle_attachments MIME whitelist ──────────────────────────────────

    fn fake_attachment(media_type: &str) -> Attachment {
        Attachment {
            path: PathBuf::from(format!("/tmp/fake-{}.bin", media_type.replace('/', "-"))),
            filename: "test".to_string(),
            media_type: media_type.to_string(),
        }
    }

    #[test]
    fn validate_attachments_accepts_png() {
        let atts = vec![fake_attachment("image/png")];
        assert!(validate_attachments(&atts).is_ok());
    }

    #[test]
    fn validate_attachments_accepts_jpeg() {
        let atts = vec![fake_attachment("image/jpeg")];
        assert!(validate_attachments(&atts).is_ok());
    }

    #[test]
    fn validate_attachments_accepts_webp() {
        let atts = vec![fake_attachment("image/webp")];
        assert!(validate_attachments(&atts).is_ok());
    }

    #[test]
    fn validate_attachments_rejects_pdf() {
        let atts = vec![fake_attachment("application/pdf")];
        match validate_attachments(&atts) {
            Err(msg) => {
                assert!(
                    msg.contains("only image attachments supported"),
                    "got: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected rejection of pdf"),
        }
    }

    #[test]
    fn validate_attachments_rejects_heic_with_chat_safe_message() {
        let atts = vec![fake_attachment("image/heic")];
        match validate_attachments(&atts) {
            Err(msg) => assert!(msg.contains("image/heic"), "got: {}", msg),
            Ok(_) => panic!("expected rejection of heic"),
        }
    }

    // ── handle_schema ──────────────────────────────────────────────────────

    #[test]
    fn handle_schema_none_returns_none() {
        let result = handle_schema(None, Path::new("/tmp"), None);
        match result {
            Ok(r) => {
                assert!(r.temp.is_none());
                assert!(r.registered_name.is_none());
            }
            Err(e) => panic!("expected Ok, got Err: {}", e),
        }
    }

    #[test]
    fn handle_schema_inline_json_writes_temp() {
        let schema = r#"{"type":"object","required":["answer"]}"#;
        let result = handle_schema(Some(schema), Path::new("/tmp"), None);
        match result {
            Ok(r) => {
                assert!(r.registered_name.is_none(), "inline must not be registered");
                let temp = r.temp.expect("inline JSON should produce a temp file");
                let p = temp.path();
                assert!(p.exists(), "temp file should exist while guard is held");
                assert!(p.extension().map(|e| e == "json").unwrap_or(false));
                let bytes = std::fs::read(p).expect("read temp");
                let s = std::str::from_utf8(&bytes).unwrap();
                assert!(s.contains("\"answer\""));
            }
            Err(e) => panic!("expected Ok, got Err: {}", e),
        }
    }

    #[test]
    fn handle_schema_invalid_json_rejected() {
        let result = handle_schema(Some("not json {"), Path::new("/tmp"), None);
        match result {
            Err(msg) => {
                assert!(
                    msg.contains("neither a file path nor valid JSON"),
                    "got: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected error on invalid json"),
        }
    }

    #[test]
    fn handle_schema_empty_string_rejected() {
        let result = handle_schema(Some("   "), Path::new("/tmp"), None);
        match result {
            Err(msg) => assert!(msg.contains("empty"), "got: {}", msg),
            Ok(_) => panic!("expected error on empty"),
        }
    }

    // ── translate_event: thread.started / turn.started ─────────────────────

    #[test]
    fn translate_thread_started_captures_thread_id() {
        let ev = serde_json::json!({
            "type": "thread.started",
            "thread_id": "019dcf4d-aaaa-7777-bbbb-cccccccccccc"
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ThreadStarted { thread_id }] => {
                assert_eq!(thread_id, "019dcf4d-aaaa-7777-bbbb-cccccccccccc");
            }
            other => panic!("expected ThreadStarted, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_started_emits_marker() {
        let ev = serde_json::json!({"type": "turn.started"});
        let out = translate_event(&ev).expect("recognized");
        assert_eq!(out, vec![TranslatedEvent::TurnStarted]);
    }

    // ── translate_event: agent_message ─────────────────────────────────────

    #[test]
    fn translate_item_completed_agent_message_emits_text() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_0",
                "type": "agent_message",
                "text": "The answer is 4."
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::AgentMessage(t)] => {
                assert_eq!(t, "The answer is 4.");
            }
            other => panic!("expected AgentMessage, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_reasoning_dropped() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {"id": "item_x", "type": "reasoning", "text": "thinking..."}
        });
        let out = translate_event(&ev).expect("recognized");
        assert!(out.is_empty(), "reasoning items must be silently dropped");
    }

    // ── translate_event: tool-kind pairing ─────────────────────────────────

    #[test]
    fn translate_item_started_command_execution_buffers_with_command() {
        let ev = serde_json::json!({
            "type": "item.started",
            "item": {
                "id": "item_1",
                "type": "command_execution",
                "command": "/bin/zsh -lc pwd",
                "aggregated_output": "",
                "exit_code": null,
                "status": "in_progress"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolUseStart {
                item_id,
                tool,
                input,
            }] => {
                assert_eq!(item_id, "item_1");
                assert_eq!(tool, "command_execution");
                assert_eq!(input.as_deref(), Some("/bin/zsh -lc pwd"));
            }
            other => panic!("expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_command_execution_emits_tool_result_success() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_1",
                "type": "command_execution",
                "command": "/bin/zsh -lc pwd",
                "aggregated_output": "/tmp/codex-spike-empty\n",
                "exit_code": 0,
                "status": "completed"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult {
                item_id,
                success,
                output,
                error,
            }] => {
                assert_eq!(item_id, "item_1");
                assert!(success, "exit_code 0 must be success");
                assert_eq!(output.as_deref(), Some("/tmp/codex-spike-empty\n"));
                assert!(error.is_none());
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_completed_command_execution_nonzero_exit_emits_error() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_1",
                "type": "command_execution",
                "command": "false",
                "aggregated_output": "",
                "exit_code": 1,
                "status": "completed"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult { success, error, .. }] => {
                assert!(!success);
                let e = error.as_deref().unwrap_or("");
                assert!(e.contains("exit_code=1"), "got: {}", e);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    // ── translate_event: terminal events ───────────────────────────────────

    #[test]
    fn translate_turn_completed_emits_terminal_marker() {
        let ev = serde_json::json!({
            "type": "turn.completed",
            "usage": {"input_tokens": 100, "output_tokens": 5}
        });
        let out = translate_event(&ev).expect("recognized");
        assert_eq!(out, vec![TranslatedEvent::TurnCompleted]);
    }

    #[test]
    fn translate_turn_failed_extracts_nested_error_message() {
        let ev = serde_json::json!({
            "type": "turn.failed",
            "error": {"message": "model rejected"}
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::TurnFailed(m)] => {
                assert_eq!(m, "model rejected");
            }
            other => panic!("expected TurnFailed, got {:?}", other),
        }
    }

    #[test]
    fn translate_turn_failed_missing_message_uses_fallback() {
        let ev = serde_json::json!({"type": "turn.failed"});
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::TurnFailed(m)] => {
                assert!(m.contains("turn failed"));
            }
            other => panic!("expected TurnFailed, got {:?}", other),
        }
    }

    #[test]
    fn translate_error_event_is_non_fatal() {
        let ev = serde_json::json!({"type": "error", "message": "transient hiccup"});
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::NonFatalError(m)] => {
                assert_eq!(m, "transient hiccup");
            }
            other => panic!("expected NonFatalError, got {:?}", other),
        }
    }

    #[test]
    fn translate_unknown_type_returns_none() {
        let ev = serde_json::json!({"type": "future_event_xyz"});
        assert!(translate_event(&ev).is_none());
    }

    #[test]
    fn translate_missing_type_returns_none() {
        let ev = serde_json::json!({"message": "no type"});
        assert!(translate_event(&ev).is_none());
    }

    // ── sanitize_stderr ────────────────────────────────────────────────────

    #[test]
    fn sanitize_stderr_filters_failed_to_record_rollout() {
        let raw = "2026-04-27T14:17:23.601357Z ERROR codex_core::session: failed to record rollout items: thread foo not found\nactual error here";
        let out = sanitize_stderr(raw);
        assert!(
            !out.contains("failed to record rollout items"),
            "should filter benign log line; got: {}",
            out
        );
        assert!(out.contains("actual error here"));
    }

    #[test]
    fn sanitize_stderr_filters_reading_additional_input() {
        let raw = "Reading additional input from stdin...\nreal stderr";
        let out = sanitize_stderr(raw);
        assert!(!out.contains("Reading additional input"), "got: {}", out);
        assert!(out.contains("real stderr"));
    }

    #[test]
    fn sanitize_stderr_redacts_env_vars() {
        let raw = "OPENAI_API_KEY=sk-secret-value123 something";
        let out = sanitize_stderr(raw);
        assert!(!out.contains("sk-secret-value123"), "got: {}", out);
        assert!(out.contains("OPENAI_API_KEY=<redacted>"));
    }

    #[test]
    fn sanitize_stderr_redacts_user_paths() {
        let raw = "error at /Users/alice/project/main.rs line 42";
        let out = sanitize_stderr(raw);
        assert!(!out.contains("alice"), "user path leaked: {}", out);
        assert!(out.contains("<redacted-path>"));
    }

    #[test]
    fn sanitize_stderr_truncates_at_500() {
        let raw = "x".repeat(700);
        let out = sanitize_stderr(&raw);
        assert!(out.chars().count() <= 500);
    }

    // ── format_panic_message ───────────────────────────────────────────────

    #[test]
    fn format_panic_message_handles_str() {
        let info: Box<dyn std::any::Any + Send> = Box::new("boom");
        let msg = format_panic_message(&*info);
        assert!(msg.contains("boom"));
        assert!(msg.contains("codex"));
    }

    #[test]
    fn format_panic_message_handles_string() {
        let info: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        let msg = format_panic_message(&*info);
        assert!(msg.contains("kaboom"));
    }

    #[test]
    fn format_panic_message_handles_unknown() {
        let info: Box<dyn std::any::Any + Send> = Box::new(42i32);
        let msg = format_panic_message(&*info);
        assert!(msg.contains("unknown"));
    }

    // ── Review-driven regression tests (post-six-pass fixes) ───────────────

    // ── Fix #1: success derivation by item type ────────────────────────────

    #[test]
    fn translate_file_change_status_completed_emits_success() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_2",
                "type": "file_change",
                "path": "/tmp/foo.rs",
                "status": "completed"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult { success, error, .. }] => {
                assert!(success, "file_change with status=completed must succeed");
                assert!(error.is_none());
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_file_change_status_failed_emits_error() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_2",
                "type": "file_change",
                "path": "/tmp/foo.rs",
                "status": "failed"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult { success, error, .. }] => {
                assert!(!success, "file_change with status!=completed must fail");
                let e = error.as_deref().unwrap_or("");
                assert!(e.contains("status=failed"), "got: {}", e);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_web_search_status_completed_emits_success() {
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_3",
                "type": "web_search",
                "query": "hello",
                "status": "completed"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult { success, .. }] => assert!(success),
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn translate_command_execution_missing_exit_code_treated_as_failure() {
        // For command_execution, the new contract is `success = exit_code == Some(0)`.
        // A missing exit_code therefore signals failure, NOT success — distinct
        // from non-command-execution kinds where status is the discriminator.
        let ev = serde_json::json!({
            "type": "item.completed",
            "item": {
                "id": "item_x",
                "type": "command_execution",
                "command": "false",
                "aggregated_output": "",
                "status": "completed"
                // exit_code intentionally absent
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolResult { success, error, .. }] => {
                assert!(
                    !success,
                    "command_execution without exit_code must NOT default to success"
                );
                assert!(
                    error
                        .as_deref()
                        .map(|e| e.contains("unknown"))
                        .unwrap_or(false),
                    "got: {:?}",
                    error
                );
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    // ── Fix #3: empty session_id falls back to --last ──────────────────────

    #[test]
    fn argv_resume_with_empty_session_id_falls_back_to_last() {
        let args = build_argv(
            "more",
            Some(""),
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            None,
        );
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "resume");
        assert!(
            args.contains(&"--last".to_string()),
            "empty session_id must produce --last fallback; got {:?}",
            args
        );
        assert!(
            !args.contains(&"".to_string()),
            "no empty positional may appear in argv; got {:?}",
            args
        );
    }

    // ── Test gap #1: position assertion on resume argv ─────────────────────

    #[test]
    fn argv_resume_session_id_strictly_precedes_prompt() {
        let args = build_argv(
            "what now",
            Some("019dcf4d-aaaa-7777-bbbb-cccccccccccc"),
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            None,
        );
        let sid_pos = args
            .iter()
            .position(|a| a == "019dcf4d-aaaa-7777-bbbb-cccccccccccc")
            .expect("session_id present");
        let prompt_pos = args
            .iter()
            .position(|a| a == "what now")
            .expect("prompt present");
        assert!(
            sid_pos < prompt_pos,
            "session_id ({}) must come strictly before prompt ({}); got {:?}",
            sid_pos,
            prompt_pos,
            args
        );
    }

    // ── argv coverage: all flags together ──────────────────────────────────

    #[test]
    fn argv_all_flags_together_have_correct_relative_order() {
        let p1 = PathBuf::from("/tmp/a.png");
        let p2 = PathBuf::from("/tmp/schema.json");
        let opts = HarnessOptions {
            model: Some("gpt-5.4".into()),
            add_dirs: vec![PathBuf::from("/tmp/lib")],
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("read-only".into()),
                profile: Some("dev".into()),
            })),
            ..Default::default()
        };
        let cfg = CodexConfig {
            ignore_user_config: true,
            ..Default::default()
        };
        let args = build_argv(
            "describe",
            None,
            &opts,
            &cfg,
            &[p1.as_path()],
            Some(p2.as_path()),
        );
        // Flags should all appear before the trailing prompt positional.
        let prompt_pos = args
            .iter()
            .position(|a| a == "describe")
            .expect("prompt present");
        for flag in [
            "--skip-git-repo-check",
            "--ignore-user-config",
            "-s",
            "-m",
            "-p",
            "--add-dir",
            "-i",
            "--output-schema",
            "--ephemeral",
        ] {
            let pos = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("flag {} missing from {:?}", flag, args));
            assert!(
                pos < prompt_pos,
                "flag {} ({}) must precede prompt ({}); got {:?}",
                flag,
                pos,
                prompt_pos,
                args
            );
        }
    }

    #[test]
    fn argv_ignore_user_config_absent_when_false() {
        let cfg = CodexConfig {
            ignore_user_config: false,
            ..Default::default()
        };
        let args = build_argv("hi", None, &HarnessOptions::default(), &cfg, &[], None);
        assert!(
            !args.contains(&"--ignore-user-config".to_string()),
            "--ignore-user-config must be absent when disabled; got {:?}",
            args
        );
    }

    #[test]
    fn argv_empty_prompt_omits_trailing_positional() {
        let args = build_argv(
            "",
            None,
            &HarnessOptions::default(),
            &CodexConfig::default(),
            &[],
            None,
        );
        // No trailing empty string; the last argv item must be a real flag/value.
        assert!(
            !args.iter().any(|a| a.is_empty()),
            "no empty positional permitted in argv; got {:?}",
            args
        );
    }

    // ── Fix #8 (post-0.128): invalid sandbox in config falls back to default ─
    //
    // Behavior change rationale: codex 0.128 deprecated `--full-auto` in favor
    // of explicit `-s <sandbox>`, and `codex exec` now requires a sandbox value
    // for the non-interactive default to be applied. Previously terminus would
    // drop the `-s` flag entirely on an invalid value and rely on `--full-auto`
    // to provide the default. With `--full-auto` gone, an invalid config value
    // must fall back to the explicit `workspace-write` default rather than
    // emit no `-s` flag at all (which would leave codex in its own default,
    // currently `read-only`-equivalent in 0.128).

    #[test]
    fn argv_invalid_sandbox_in_config_falls_back_to_workspace_write() {
        let cfg = CodexConfig {
            sandbox: Some("banana".into()),
            ..Default::default()
        };
        let args = build_argv("hi", None, &HarnessOptions::default(), &cfg, &[], None);
        assert!(
            !args.iter().any(|a| a == "banana"),
            "invalid sandbox must NOT appear in argv; got {:?}",
            args
        );
        let s_pos = args
            .iter()
            .position(|a| a == "-s")
            .expect("expected -s flag with workspace-write fallback; got {:?}");
        assert_eq!(
            args.get(s_pos + 1).map(String::as_str),
            Some("workspace-write"),
            "invalid value should fall back to workspace-write; got {:?}",
            args
        );
    }

    #[test]
    fn argv_invalid_sandbox_in_options_per_prompt_falls_back() {
        // Per-prompt sandbox is validated by the parser, not build_argv. If
        // somehow an invalid value reaches build_argv via options (bypassing
        // the parser), it should also fall back to workspace-write — defense
        // in depth.
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("not-a-real-policy".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let args = build_argv("hi", None, &opts, &CodexConfig::default(), &[], None);
        assert!(!args.iter().any(|a| a == "not-a-real-policy"));
        let s_pos = args
            .iter()
            .position(|a| a == "-s")
            .expect("expected -s flag with fallback");
        assert_eq!(
            args.get(s_pos + 1).map(String::as_str),
            Some("workspace-write"),
            "invalid options.sandbox should fall back; got {:?}",
            args
        );
    }

    // ── Fix #4 + #7: handle_schema validation + directory error ────────────

    #[test]
    fn handle_schema_existing_file_with_valid_json_writes_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schema.json");
        std::fs::write(&path, r#"{"type":"object"}"#).expect("write");
        let result = handle_schema(Some(path.to_str().unwrap()), dir.path(), None);
        match result {
            Ok(r) => {
                let temp = r.temp.expect("file path should produce a temp");
                let bytes = std::fs::read(temp.path()).expect("read");
                let s = std::str::from_utf8(&bytes).unwrap();
                assert!(s.contains("\"object\""), "got: {}", s);
                assert!(r.registered_name.is_none());
            }
            Err(e) => panic!("expected Ok, got Err: {}", e),
        }
    }

    #[test]
    fn handle_schema_existing_file_with_invalid_json_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-json.txt");
        std::fs::write(&path, "this is not json").expect("write");
        let result = handle_schema(Some(path.to_str().unwrap()), dir.path(), None);
        match result {
            Err(msg) => {
                assert!(
                    msg.contains("schema file is not valid JSON"),
                    "got: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected validation error"),
        }
    }

    #[test]
    fn handle_schema_directory_path_emits_explicit_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = handle_schema(Some(dir.path().to_str().unwrap()), dir.path(), None);
        match result {
            Err(msg) => {
                assert!(
                    msg.contains("path exists but is not a file"),
                    "expected explicit directory-path error; got: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected directory rejection"),
        }
    }

    #[test]
    fn handle_schema_existing_file_with_non_utf8_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x00]).expect("write");
        let result = handle_schema(Some(path.to_str().unwrap()), dir.path(), None);
        match result {
            Err(msg) => assert!(msg.contains("UTF-8"), "got: {}", msg),
            Ok(_) => panic!("expected UTF-8 rejection"),
        }
    }

    // ── F11: registered-schema name resolution ──────────────────────────────

    fn registry_with(
        name: &str,
        webhook: Option<&str>,
        env: Option<&str>,
    ) -> Arc<crate::structured_output::SchemaRegistry> {
        // Helper: build a registry with one schema entry. When `webhook` is
        // Some, `env` must also be Some and that env var must be set; the
        // registry validation will fail otherwise.
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            name.to_string(),
            crate::config::SchemaEntry {
                schema: r#"{"type":"object","required":["answer"]}"#.to_string(),
                webhook: webhook.map(|s| s.to_string()),
                webhook_secret_env: env.map(|s| s.to_string()),
            },
        );
        Arc::new(crate::structured_output::SchemaRegistry::from_config(&entries).expect("registry"))
    }

    #[test]
    fn argv_schema_registered_name_resolved_via_registry() {
        let reg = registry_with("todos_v1", None, None);
        let resolved = handle_schema(Some("todos_v1"), Path::new("/tmp"), Some(&reg))
            .expect("registered name should resolve");
        assert_eq!(resolved.registered_name.as_deref(), Some("todos_v1"));
        let temp = resolved
            .temp
            .expect("registered schema produces a temp file");
        let bytes = std::fs::read(temp.path()).expect("read");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("\"answer\""),
            "temp must hold the registered schema's value, got: {}",
            s
        );
    }

    #[test]
    fn registered_name_takes_priority_over_inline_json() {
        // When a registry contains a name that ALSO happens to parse as JSON,
        // the registered form wins. This is the security-relevant ordering:
        // a malicious user can't bypass the registry by passing inline JSON
        // with the same shape.
        let reg = registry_with("inline_v1", None, None);
        // The argument here is just the schema name; if priority were
        // reversed, this would fall through to inline-JSON path and fail
        // because "inline_v1" is not valid JSON.
        let resolved = handle_schema(Some("inline_v1"), Path::new("/tmp"), Some(&reg))
            .expect("registered name should resolve first");
        assert_eq!(resolved.registered_name.as_deref(), Some("inline_v1"));
    }

    #[test]
    fn unknown_registered_name_falls_through_to_inline() {
        let reg = registry_with("known_v1", None, None);
        // "unknown_v1" is not in the registry; if it were ALSO not valid
        // JSON, we'd get the inline-parse-error path. Pass a JSON-ish name
        // to verify the fall-through works.
        let inline = r#"{"type":"object"}"#;
        let resolved = handle_schema(Some(inline), Path::new("/tmp"), Some(&reg))
            .expect("inline JSON should still work when registry exists");
        assert!(resolved.registered_name.is_none());
        assert!(resolved.temp.is_some());
    }

    #[test]
    fn inline_json_schema_unchanged_emits_text() {
        // Regression guard: inline-JSON schemas (the v1 form) are NEVER
        // routed through the structured-output / webhook path even when a
        // registry is attached. They produce a temp file with no
        // `registered_name`, so `decide_structured_schema` returns None.
        let reg = registry_with("known_v1", Some("https://e.example/wh"), Some("HOME"));
        let inline = r#"{"type":"object","properties":{"x":{"type":"string"}}}"#;
        let resolved = handle_schema(Some(inline), Path::new("/tmp"), Some(&reg))
            .expect("inline JSON should resolve");
        assert!(
            resolved.registered_name.is_none(),
            "inline JSON must never carry a registered_name"
        );
        assert!(
            decide_structured_schema(resolved.registered_name.as_deref(), Some(&reg)).is_none(),
            "inline JSON must never trigger structured-output emission"
        );
    }

    #[test]
    fn run_prompt_emits_structured_output_when_schema_registered_with_webhook() {
        // Unit-level surrogate for the integration test. We can't spawn
        // codex in a unit test, but we CAN pin the decision logic:
        // `decide_structured_schema` returns Some(name) iff the registry
        // has a webhook configured for that name.
        // HOME is a guaranteed-set env var in tests; using it spares the
        // test from setting/unsetting custom env vars.
        let reg = registry_with("orders_v1", Some("https://e.example/wh"), Some("HOME"));
        assert_eq!(
            decide_structured_schema(Some("orders_v1"), Some(&reg)),
            Some("orders_v1".to_string()),
            "registered name + webhook → StructuredOutput emission"
        );
    }

    #[test]
    fn run_prompt_falls_back_to_text_when_schema_registered_without_webhook() {
        let reg = registry_with("orders_v1", None, None);
        assert_eq!(
            decide_structured_schema(Some("orders_v1"), Some(&reg)),
            None,
            "registered name without webhook → fall back to Text"
        );
    }

    #[test]
    fn registered_name_priority_documented_in_inline_error() {
        // When inline-JSON parse fails AND a registry is attached, the
        // error should list the registered names so users discover the
        // shortcut.
        let reg = registry_with("orders_v1", None, None);
        let err = handle_schema(Some("not json {"), Path::new("/tmp"), Some(&reg))
            .expect_err("invalid input should error");
        assert!(
            err.contains("orders_v1"),
            "error should mention registered schemas; got: {}",
            err
        );
    }

    /// Critic-4 P2.5: registered-schemas hint in the parse-failure error
    /// must cap at 5 names to avoid unreadable 500-character error lines
    /// when a user has 50 schemas configured.
    #[test]
    fn registered_schemas_hint_caps_at_five_with_more_suffix() {
        // Registry with 7 schemas — we expect the first 5 listed plus
        // "…and 2 more".
        let mut entries = std::collections::HashMap::new();
        for i in 0..7 {
            entries.insert(
                format!("schema_{:02}", i),
                crate::config::SchemaEntry {
                    schema: r#"{"type":"object"}"#.to_string(),
                    webhook: None,
                    webhook_secret_env: None,
                },
            );
        }
        let reg = Arc::new(
            crate::structured_output::SchemaRegistry::from_config(&entries).expect("registry"),
        );
        let err = handle_schema(Some("not json {"), Path::new("/tmp"), Some(&reg))
            .expect_err("invalid input should error");
        assert!(
            err.contains("…and 2 more"),
            "expected '…and N more' suffix when >5 schemas registered, got: {}",
            err
        );
        // No more than 5 schema names appear before the "…and" suffix.
        let listed = err.matches("schema_").count();
        assert!(
            listed <= 5,
            "at most 5 schemas should appear in the hint, got {} in: {}",
            listed,
            err
        );
    }

    /// Critic-6 P1.2: the "directory or special file" error must NOT echo
    /// the resolved cwd-joined path back to chat (it would leak the
    /// server's cwd to whoever sent the prompt). Echo only the user's
    /// raw input string.
    #[test]
    fn directory_path_error_does_not_leak_resolved_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        // User passes a relative name that resolves to a directory under
        // the test cwd. The cwd path must NOT appear in the error.
        let err = handle_schema(Some("subdir"), dir.path(), None);
        // Ensure it's actually a directory case: create the dir.
        std::fs::create_dir_all(dir.path().join("subdir")).expect("mkdir");
        let err = handle_schema(Some("subdir"), dir.path(), None)
            .err()
            .or(err.err())
            .expect("directory should produce an error");
        // The cwd's full path must NOT appear in the error.
        let cwd_str = dir.path().display().to_string();
        assert!(
            !err.contains(&cwd_str),
            "resolved cwd path leaked to chat: {}\ncwd: {}",
            err,
            cwd_str
        );
        // The user's raw input is allowed (and expected) in the error.
        assert!(
            err.contains("subdir"),
            "raw input should appear in error; got: {}",
            err
        );
    }

    // ── Test gaps: validate_attachments edge cases ─────────────────────────

    #[test]
    fn validate_attachments_empty_list_is_ok() {
        assert!(validate_attachments(&[]).is_ok());
    }

    #[test]
    fn validate_attachments_mixed_valid_and_invalid_rejects_whole_batch() {
        let atts = vec![
            fake_attachment("image/png"),
            fake_attachment("application/pdf"),
        ];
        assert!(validate_attachments(&atts).is_err());
    }

    #[test]
    fn validate_attachments_uppercase_mime_accepted() {
        // Fix #11: case-insensitive MIME comparison.
        let atts = vec![fake_attachment("Image/PNG")];
        assert!(
            validate_attachments(&atts).is_ok(),
            "Image/PNG must be accepted"
        );
    }

    #[test]
    fn validate_attachments_image_jpg_alternative_spelling_accepted() {
        let atts = vec![fake_attachment("image/jpg")];
        assert!(validate_attachments(&atts).is_ok());
    }

    // ── Fix #9: sanitize_stderr full-value redaction ───────────────────────

    #[test]
    fn sanitize_stderr_redacts_full_value_until_eol() {
        // Quoted values containing spaces must NOT leak the post-space portion.
        let raw = "OPENAI_API_KEY='secret with spaces' more after\nnext line";
        let out = sanitize_stderr(raw);
        assert!(!out.contains("secret"), "secret leaked: {}", out);
        assert!(!out.contains("with spaces"), "value leaked: {}", out);
        assert!(out.contains("OPENAI_API_KEY=<redacted>"));
        assert!(out.contains("next line"), "non-redacted lines must survive");
    }

    // ── Test gaps: sanitize_stderr edge cases ──────────────────────────────

    #[test]
    fn sanitize_stderr_empty_input_returns_empty() {
        assert_eq!(sanitize_stderr(""), "");
    }

    #[test]
    fn sanitize_stderr_only_benign_lines_produces_empty() {
        let raw = "Reading additional input from stdin...\n2026-04-27 ERROR codex_core::session: failed to record rollout items: thread foo not found";
        let out = sanitize_stderr(raw);
        assert!(
            out.trim().is_empty(),
            "expected empty after benign filter; got: {:?}",
            out
        );
    }

    #[test]
    fn sanitize_stderr_benign_line_with_secret_is_fully_suppressed() {
        // Cross-cut: a line that matches the benign filter AND contains a
        // secret is dropped entirely. The secret never reaches the env-var
        // redaction pass — and that's fine because the whole line is gone.
        let raw = "OPENAI_KEY=sekret failed to record rollout items: thread x not found\nlegitimate error here";
        let out = sanitize_stderr(raw);
        assert!(!out.contains("sekret"), "secret leaked: {}", out);
        assert!(
            !out.contains("failed to record"),
            "benign line leaked: {}",
            out
        );
        assert!(
            out.contains("legitimate error"),
            "real error suppressed: {}",
            out
        );
    }

    // ── translate_item_started: tool kinds ─────────────────────────────────

    #[test]
    fn translate_item_started_file_change_buffers_with_path() {
        let ev = serde_json::json!({
            "type": "item.started",
            "item": {
                "id": "item_5",
                "type": "file_change",
                "path": "/tmp/edited.rs",
                "status": "in_progress"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolUseStart {
                item_id,
                tool,
                input,
            }] => {
                assert_eq!(item_id, "item_5");
                assert_eq!(tool, "file_change");
                assert_eq!(input.as_deref(), Some("/tmp/edited.rs"));
            }
            other => panic!("expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_web_search_buffers_with_query() {
        let ev = serde_json::json!({
            "type": "item.started",
            "item": {
                "id": "item_6",
                "type": "web_search",
                "query": "rust async tokio",
                "status": "in_progress"
            }
        });
        let out = translate_event(&ev).expect("recognized");
        match &out[..] {
            [TranslatedEvent::ToolUseStart { tool, input, .. }] => {
                assert_eq!(tool, "web_search");
                assert_eq!(input.as_deref(), Some("rust async tokio"));
            }
            other => panic!("expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn translate_item_started_agent_message_silently_dropped() {
        // agent_message and reasoning items only appear at item.completed.
        // An item.started with type=agent_message should be silently dropped
        // (empty Vec, NOT None — the event type IS recognized).
        let ev = serde_json::json!({
            "type": "item.started",
            "item": {"id": "item_7", "type": "agent_message", "text": "thinking"}
        });
        let out = translate_event(&ev).expect("recognized");
        assert!(
            out.is_empty(),
            "agent_message item.started must be dropped; got {:?}",
            out
        );
    }

    // ── chat-safe subcommand passthrough (F6 + F7) ──────────────────────────

    #[test]
    fn build_subcommand_argv_sessions_uses_resume_all() {
        let argv = build_subcommand_argv(&CodexSubcommand::Sessions, &[]);
        assert_eq!(argv, vec!["resume".to_string(), "--all".to_string()]);
    }

    #[test]
    fn build_subcommand_argv_apply_threads_task_id() {
        let argv = build_subcommand_argv(&CodexSubcommand::Apply, &["task_abc123".to_string()]);
        assert_eq!(argv, vec!["apply".to_string(), "task_abc123".to_string()]);
    }

    #[test]
    fn build_subcommand_argv_cloud_list_no_args() {
        let argv = build_subcommand_argv(&CodexSubcommand::Cloud(CodexCloudSubcommand::List), &[]);
        assert_eq!(argv, vec!["cloud".to_string(), "list".to_string()]);
    }

    #[test]
    fn build_subcommand_argv_cloud_status_with_id() {
        let argv = build_subcommand_argv(
            &CodexSubcommand::Cloud(CodexCloudSubcommand::Status),
            &["task_xyz".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "cloud".to_string(),
                "status".to_string(),
                "task_xyz".to_string()
            ]
        );
    }

    #[test]
    fn build_subcommand_argv_cloud_diff_with_id() {
        let argv = build_subcommand_argv(
            &CodexSubcommand::Cloud(CodexCloudSubcommand::Diff),
            &["task_qq".to_string()],
        );
        assert_eq!(argv[0], "cloud");
        assert_eq!(argv[1], "diff");
        assert_eq!(argv[2], "task_qq");
    }

    #[test]
    fn build_subcommand_argv_cloud_apply_with_id() {
        let argv = build_subcommand_argv(
            &CodexSubcommand::Cloud(CodexCloudSubcommand::Apply),
            &["task_q".to_string()],
        );
        assert_eq!(argv, vec!["cloud", "apply", "task_q"]);
    }

    #[test]
    fn build_subcommand_argv_cloud_exec_threads_full_args() {
        let argv = build_subcommand_argv(
            &CodexSubcommand::Cloud(CodexCloudSubcommand::Exec),
            &[
                "--env".into(),
                "env_main".into(),
                "fix".into(),
                "auth".into(),
            ],
        );
        assert_eq!(argv[0], "cloud");
        assert_eq!(argv[1], "exec");
        assert_eq!(argv[2], "--env");
        assert_eq!(argv[3], "env_main");
        assert_eq!(argv[4], "fix");
        assert_eq!(argv[5], "auth");
    }

    #[test]
    fn sub_display_name_covers_all_variants() {
        // Defends against a future enum-variant addition that forgets to
        // extend sub_display_name. Uses string equality (not the enum) so
        // that the chat-facing labels are pinned.
        assert_eq!(sub_display_name(&CodexSubcommand::Sessions), "sessions");
        assert_eq!(sub_display_name(&CodexSubcommand::Apply), "apply");
        assert_eq!(
            sub_display_name(&CodexSubcommand::Cloud(CodexCloudSubcommand::List)),
            "cloud list"
        );
        assert_eq!(
            sub_display_name(&CodexSubcommand::Cloud(CodexCloudSubcommand::Status)),
            "cloud status"
        );
        assert_eq!(
            sub_display_name(&CodexSubcommand::Cloud(CodexCloudSubcommand::Diff)),
            "cloud diff"
        );
        assert_eq!(
            sub_display_name(&CodexSubcommand::Cloud(CodexCloudSubcommand::Apply)),
            "cloud apply"
        );
        assert_eq!(
            sub_display_name(&CodexSubcommand::Cloud(CodexCloudSubcommand::Exec)),
            "cloud exec"
        );
    }

    /// Mirror of `gemini.rs::run_subcommand_emits_done_after_panic`: when the
    /// subprocess body panics the spawned task must still emit a terminal Done
    /// after the Error so the receiver doesn't hang. We force a spawn failure
    /// by pointing the binary at a missing path; the spawn-failure arm
    /// produces the same Error+Done pair as a panic. The path mirrors the
    /// gemini convention (`/nonexistent/...-cN`) so the test reliably hits
    /// `ErrorKind::NotFound` on both macOS and Linux. (Test-engineer P1.)
    #[tokio::test]
    async fn run_subcommand_spawn_failure_emits_error_then_done() {
        let cfg = CodexConfig {
            binary_path: Some(PathBuf::from("/nonexistent/codex-binary-for-tests-c2a9")),
            ..Default::default()
        };
        let h = CodexHarness::new_with_config(cfg);
        let mut rx = h
            .run_subcommand(CodexSubcommand::Sessions, vec![])
            .await
            .expect("run_subcommand returns receiver");
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert_eq!(
            events.len(),
            2,
            "expected exactly Error+Done; got {:?}",
            events
        );
        match &events[0] {
            HarnessEvent::Error(msg) => {
                assert!(
                    msg.contains("codex binary not found") || msg.contains("spawn failed"),
                    "unexpected error message: {}",
                    msg
                );
            }
            other => panic!("expected Error first, got {:?}", other),
        }
        assert!(
            matches!(events[1], HarnessEvent::Done { .. }),
            "expected Done last, got {:?}",
            events[1]
        );
    }
}
