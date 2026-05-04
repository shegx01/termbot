//! Subprocess lifecycle helpers shared by the three CLI-subprocess harnesses
//! (`opencode`, `gemini`, `codex`). The `claude` harness uses the
//! `claude-agent-sdk-rust` crate (no subprocess) and does not call any of
//! these.
//!
//! The helpers below assume they're called from inside a
//! `tokio::spawn(async move { … })` task. None of them spawn the task on the
//! caller's behalf; the caller owns the spawn so it can move per-harness
//! state into the closure freely.
//!
//! # When to add a helper here vs in `mod.rs`
//!
//! `mod.rs` hosts platform-agnostic foundation helpers
//! (`HarnessKind::cli_label`, `format_panic_message`, `strip_ansi`,
//! `SessionStore`, `truncate`, `sanitize_stderr_base`,
//! `format_tool_event_full`) and the consumer-side stream driver
//! (`drive_harness`, `HarnessContext`).
//!
//! `lifecycle.rs` (this file) hosts subprocess-process-lifecycle helpers:
//! anything tightly coupled to spawning, draining, or reaping a child
//! process. The rule of thumb:
//!
//! - If a helper builds, spawns, awaits, or kills a `tokio::process::Child`,
//!   it goes here.
//! - If a helper consumes the I/O streams produced by such a spawn (e.g.
//!   `collect_stderr` awaits a stderr drain task that `spawn_subprocess`
//!   spawned), it goes here too — the consumer cannot be understood without
//!   the producer it pairs with.
//! - If a helper is a pure transformation on text/types/enums, or a
//!   stream-consumer that doesn't pair with one of the spawn functions
//!   here, it goes in `mod.rs`.

use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::{format_panic_message, strip_ansi, HarnessEvent, HarnessKind};
use crate::socket::events::AmbientEvent;

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

/// Emit `Error` followed by `Done` on `tx`. Used by all subprocess-failure
/// paths so the Error+Done pair can't drift apart.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::sanitize_stderr_base;

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
}
