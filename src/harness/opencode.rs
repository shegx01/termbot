//! OpenCode harness — runs prompts through an `opencode serve` sidecar via
//! the `opencode-client-sdk` crate.
//!
//! # Architecture
//!
//! A single long-lived sidecar process is lazy-started on first use and kept
//! alive for the lifetime of terminus. Each prompt runs in a spawned task:
//! the task resolves (or creates) a session id, POSTs the user prompt, and
//! translates the SSE event stream into `HarnessEvent`s on an `mpsc::channel`.
//!
//! # Tool-event accumulator
//!
//! opencode's SSE surface splits tool events into `running` + `completed`
//! frames keyed by `callID`. We hold partial state in an in-task `HashMap`
//! and emit exactly one `HarnessEvent::ToolUse` per call on the terminal
//! transition (Completed, Error, or stream-close drain).
//!
//! # Panic safety
//!
//! The prompt-body future is wrapped in `AssertUnwindSafe(..).catch_unwind()`;
//! a panic is caught and surfaced as `HarnessEvent::Error` before the channel
//! closes, matching the `ClaudeHarness` pattern in `src/harness/claude.rs`.

use super::{Attachment, Harness, HarnessEvent, HarnessKind};
use crate::command::HarnessOptions;
use crate::config::OpencodeConfig;
use crate::socket::events::AmbientEvent;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt};
use opencode::{
    client::{OpencodeClient, OpencodeClientConfig, RequestOptions, SseStream},
    create_opencode_client, create_opencode_server, Error as SdkError, OpencodeServer,
    OpencodeServerOptions,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// Owned sidecar handle + bound HTTP client. Held inside
/// `Arc<Mutex<Option<Sidecar>>>` so the prompt path and shutdown path can
/// coordinate without holding the lock across SSE streams.
struct Sidecar {
    // Kept alive to own the child process (its Drop sends SIGKILL);
    // `shutdown()` uses `server.close()` for graceful termination.
    #[allow(dead_code)]
    server: OpencodeServer,
    client: OpencodeClient,
}

/// OpenCode harness: wraps the `opencode` CLI sidecar.
pub struct OpencodeHarness {
    config: OpencodeConfig,
    sidecar: Arc<Mutex<Option<Sidecar>>>,
    /// Name → session ULID. Backed by a std `Mutex<HashMap>` (no `dashmap` dep).
    sessions: Arc<StdMutex<HashMap<String, String>>>,
    /// Per-ULID serialization locks so concurrent prompts for the same
    /// session queue instead of racing.
    session_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Optional ambient bus for lifecycle events (set by App; `None` in tests).
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
}

impl OpencodeHarness {
    /// Preferred constructor with ambient event bus.
    pub fn new(config: OpencodeConfig, ambient_tx: broadcast::Sender<AmbientEvent>) -> Self {
        warn_on_unsupported_config(&config);
        Self {
            config,
            sidecar: Arc::new(Mutex::new(None)),
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            ambient_tx: Some(ambient_tx),
        }
    }

    /// Fallback constructor for tests that don't wire up the ambient bus.
    #[allow(dead_code)]
    pub fn new_with_config(config: OpencodeConfig) -> Self {
        warn_on_unsupported_config(&config);
        Self {
            config,
            sidecar: Arc::new(Mutex::new(None)),
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            ambient_tx: None,
        }
    }

    /// Shut down the sidecar. Bounded by a 2s timeout; on timeout the Drop
    /// impl on `OpencodeServer` sends SIGKILL.
    ///
    /// Not a `Harness` trait method — called from the main-loop ctrl_c branch
    /// on a named `Arc<OpencodeHarness>` held by `App`.
    pub async fn shutdown(&self) {
        let mut guard = self.sidecar.lock().await;
        if let Some(mut sc) = guard.take() {
            match tokio::time::timeout(Duration::from_secs(2), sc.server.close()).await {
                Ok(Ok(())) => info!("opencode sidecar shut down cleanly"),
                Ok(Err(e)) => warn!("opencode sidecar close returned error: {}", e),
                Err(_) => {
                    warn!(
                        "opencode sidecar shutdown timed out after 2s; dropping (SIGKILL via Drop)"
                    );
                    drop(sc); // Drop impl on OpencodeServer sends SIGKILL
                }
            }
        }
    }
}

/// Format a `catch_unwind` payload into a chat-friendly error message.
/// Extracted so the panic-path behavior is unit-testable without a live
/// sidecar (Fix C3-MEDIUM-5). Handles the three common `Box<dyn Any>`
/// payload shapes: `&str`, `String`, and everything else (unknown).
fn format_panic_message(panic_info: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic_info.downcast_ref::<&str>() {
        format!("opencode: internal panic: {}", s)
    } else if let Some(s) = panic_info.downcast_ref::<String>() {
        format!("opencode: internal panic: {}", s)
    } else {
        "opencode: internal panic (unknown)".to_string()
    }
}

/// Warn once on unsupported config fields (eager start_mode, custom crash_policy)
/// so users aren't silently ignored. See Fix C1-MAJOR-1.
fn warn_on_unsupported_config(config: &OpencodeConfig) {
    if matches!(
        config.start_mode,
        Some(crate::config::OpencodeStartMode::Eager)
    ) {
        warn!("opencode: eager start_mode not yet implemented; falling back to lazy");
    }
    if matches!(
        config.crash_policy,
        Some(crate::config::OpencodeCrashPolicy::Restart)
    ) {
        warn!("opencode: restart crash_policy not yet implemented; falling back to error");
    }
}

/// Map the SDK's `Error` into a chat-friendly `anyhow!` per the plan's
/// operational-edges UX (C5).
fn map_sidecar_error(err: SdkError, config: &OpencodeConfig, port: u16) -> anyhow::Error {
    match &err {
        SdkError::ServerStartupTimeout { .. } => {
            anyhow!("opencode sidecar did not start within 5s")
        }
        SdkError::CLINotFound(ref cli) => {
            if let Some(ref p) = config.binary_path {
                anyhow!("opencode binary at {:?} not executable: {}", p, cli.message)
            } else {
                anyhow!("opencode not on PATH")
            }
        }
        SdkError::Io(ref io) => {
            let k = io.kind();
            if k == std::io::ErrorKind::AddrInUse {
                anyhow!(
                    "127.0.0.1:{} in use, check for another opencode instance",
                    port
                )
            } else if k == std::io::ErrorKind::NotFound && config.binary_path.is_none() {
                anyhow!("opencode not on PATH")
            } else if let Some(ref p) = config.binary_path {
                anyhow!("opencode binary at {:?} not executable: {}", p, io)
            } else {
                anyhow!("opencode sidecar did not start within 5s (io: {})", io)
            }
        }
        other => {
            // Fall back to substring match on the rendered error — the SDK
            // doesn't always surface `AddrInUse` via the Io variant.
            let msg = other.to_string();
            if msg.contains("address already in use")
                || msg.contains("EADDRINUSE")
                || msg.contains("Address already in use")
            {
                anyhow!(
                    "127.0.0.1:{} in use, check for another opencode instance",
                    port
                )
            } else {
                error!("opencode sidecar spawn failed: {}", msg);
                anyhow!("opencode sidecar did not start within 5s")
            }
        }
    }
}

/// Accumulated per-call tool state. One entry is created on the first
/// `running`-like event and removed when the terminal event arrives (or
/// on stream-close drain).
#[derive(Debug, Clone, Default)]
struct PartialToolPart {
    tool: String,
    input: Option<String>,
    description: String,
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
        _attachments: &[Attachment],
        _cwd: &Path,
        session_id: Option<&str>,
        options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(256);

        let prompt = prompt.to_string();
        let resume_id = session_id.map(|s| s.to_string());
        let harness_opts = options.clone();
        let sidecar = Arc::clone(&self.sidecar);
        let sessions = Arc::clone(&self.sessions);
        let session_locks = Arc::clone(&self.session_locks);
        let ambient_tx = self.ambient_tx.clone();
        let config = self.config.clone();

        // Clone the receiver/mutex handles we need inside the spawned body
        // (for panic-time cleanup).
        let sidecar_for_panic = Arc::clone(&sidecar);
        let sessions_for_panic = Arc::clone(&sessions);
        let ambient_tx_for_panic = ambient_tx.clone();

        tokio::spawn(async move {
            let body_tx = event_tx.clone();
            let result = AssertUnwindSafe(run_opencode_prompt_inner(
                prompt,
                resume_id,
                harness_opts,
                sidecar,
                sessions,
                session_locks,
                ambient_tx,
                config,
                body_tx,
            ))
            .catch_unwind()
            .await;

            if let Err(panic_info) = result {
                let msg = format_panic_message(panic_info);
                let _ = event_tx.send(HarnessEvent::Error(msg.clone())).await;
                if let Some(ref tx) = ambient_tx_for_panic {
                    let _ = tx.send(AmbientEvent::HarnessFinished {
                        harness: HarnessKind::Opencode.name().to_string(),
                        run_id: String::new(),
                        status: "error".to_string(),
                    });
                }
                // Clear the sidecar handle so the next prompt respawns.
                {
                    let mut g = sidecar_for_panic.lock().await;
                    *g = None;
                }
                if let Ok(mut s) = sessions_for_panic.lock() {
                    s.clear();
                }
            }
        });

        Ok(event_rx)
    }

    fn get_session_id(&self, session_name: &str) -> Option<String> {
        self.sessions
            .lock()
            .ok()
            .and_then(|m| m.get(session_name).cloned())
    }

    fn set_session_id(&self, session_name: &str, id: String) {
        if let Ok(mut m) = self.sessions.lock() {
            m.insert(session_name.to_string(), id);
        }
    }
}

/// Forward `Harness` impls on an `Arc<OpencodeHarness>` so `App` can hold a
/// strong named reference (`Arc<OpencodeHarness>`) *and* insert a cloned
/// handle into the trait-object map without double-boxing.
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

    fn set_session_id(&self, session_name: &str, id: String) {
        (**self).set_session_id(session_name, id)
    }
}

/// Inner prompt body. Kept as a free function so the call site can wrap it in
/// `AssertUnwindSafe(..).catch_unwind()`.
#[allow(clippy::too_many_arguments)]
async fn run_opencode_prompt_inner(
    prompt: String,
    resume_id: Option<String>,
    _harness_opts: HarnessOptions,
    sidecar: Arc<Mutex<Option<Sidecar>>>,
    sessions: Arc<StdMutex<HashMap<String, String>>>,
    session_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    ambient_tx: Option<broadcast::Sender<AmbientEvent>>,
    config: OpencodeConfig,
    event_tx: mpsc::Sender<HarnessEvent>,
) {
    // --- 1. Progress-indicator race against the lazy sidecar start. ---
    //
    // If `ensure_sidecar_via` takes more than 1s, surface a Text event so the
    // user knows we're still working.  We deliberately do NOT cancel the
    // spawn — the indicator is just a hint.
    let client = {
        let fast = ensure_sidecar_via(&sidecar, &config);
        tokio::pin!(fast);
        tokio::select! {
            r = &mut fast => r,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                let _ = event_tx
                    .send(HarnessEvent::Text("Starting opencode sidecar...".to_string()))
                    .await;
                fast.await
            }
        }
    };
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            let _ = event_tx.send(HarnessEvent::Error(format!("{:#}", e))).await;
            if let Some(ref tx) = ambient_tx {
                let _ = tx.send(AmbientEvent::HarnessFinished {
                    harness: HarnessKind::Opencode.name().to_string(),
                    run_id: String::new(),
                    status: "error".to_string(),
                });
            }
            return;
        }
    };

    // --- 2. Resolve or create the session id. ---
    let session_id = match resume_id.clone() {
        Some(id) => id,
        None => match create_session(&client).await {
            Ok(id) => id,
            Err(e) => {
                let _ = event_tx
                    .send(HarnessEvent::Error(format!(
                        "opencode: failed to create session: {:#}",
                        e
                    )))
                    .await;
                return;
            }
        },
    };

    // --- 3. Emit ambient HarnessStarted (if bus is wired). ---
    emit_started(&ambient_tx, &session_id);

    // --- 4. Per-session serialization lock. ---
    let lock = {
        let mut locks = session_locks.lock().await;
        locks
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // --- 5. POST the prompt + subscribe to the event stream. ---
    //
    // We start the SSE subscription BEFORE the POST so we don't race and drop
    // the first events. SDK's `/event` stream is global — we filter by
    // `sessionID` client-side.
    let sse = match client.event().subscribe(RequestOptions::default()).await {
        Ok(s) => s,
        Err(e) => {
            // Clear cached sidecar only on transport-level failures; API-level
            // errors (4xx) mean the sidecar is alive.
            // SSE subscribe failures are very likely transport-level (the stream
            // couldn't even open), so clear unconditionally here.
            {
                let mut g = sidecar.lock().await;
                *g = None;
            }
            if let Ok(mut s) = sessions.lock() {
                s.retain(|_name, id| id != &session_id);
            }
            let _ = event_tx
                .send(HarnessEvent::Error(format!(
                    "opencode: failed to subscribe to event stream: {}",
                    e
                )))
                .await;
            emit_finish(&ambient_tx, &session_id, "error");
            return;
        }
    };

    // POST the prompt (fire-and-forget w.r.t. the response body; events come
    // over SSE).  Part shape mirrors the official JS SDK: an array of Parts,
    // with a single text part for a plain prompt.
    let post_body = json!({
        "parts": [{ "type": "text", "text": prompt }],
    });
    let post_opts = RequestOptions::default()
        .with_path("sessionID", &session_id)
        .with_body(post_body);

    if let Err(e) = client.session().prompt(post_opts).await {
        // Clear cached sidecar only on transport-level failures; API-level errors
        // (4xx) mean the sidecar is alive — prompt too long, quota, etc.
        let is_transport_failure = matches!(
            e,
            SdkError::Http(_)
                | SdkError::Io(_)
                | SdkError::ServerStartupTimeout { .. }
                | SdkError::Process(_)
                | SdkError::CLINotFound(_)
                | SdkError::InvalidHeaderValue(_)
                | SdkError::InvalidHeaderName(_)
        );
        if is_transport_failure {
            // Sidecar is likely dead; clear handle so next prompt respawns.
            {
                let mut g = sidecar.lock().await;
                *g = None;
            }
            if let Ok(mut s) = sessions.lock() {
                s.retain(|_name, id| id != &session_id);
            }
        }
        // Api / Json / MissingPathParameter / OpencodeSDK: sidecar responded,
        // just this request failed. Keep the sidecar alive.
        let _ = event_tx
            .send(HarnessEvent::Error(format!(
                "opencode: POST /session/{}/message failed: {}",
                session_id, e
            )))
            .await;
        emit_finish(&ambient_tx, &session_id, "error");
        return;
    }

    // --- 6. Translate SSE events → HarnessEvents. ---
    let mut accumulator: HashMap<String, PartialToolPart> = HashMap::new();
    let mut sidecar_died = false;
    let mut saw_recognized_event = false;
    let mut saw_unknown_event = false;

    process_sse_stream(
        sse,
        &session_id,
        &event_tx,
        &mut accumulator,
        &mut sidecar_died,
        &mut saw_recognized_event,
        &mut saw_unknown_event,
    )
    .await;

    // Version-compatibility signal: if the sidecar produced a stream with
    // only unknown event names (no Text / ToolUse emitted), surface that to
    // the user. Silent empty output is worse than an actionable warning
    // pointing at a likely opencode version mismatch. (Fix C2-MAJOR-3)
    if !sidecar_died && saw_unknown_event && !saw_recognized_event {
        let _ = event_tx
            .send(HarnessEvent::Text(
                "opencode: no recognized events received — opencode version may be incompatible"
                    .to_string(),
            ))
            .await;
    }

    // --- 7. Drain any orphaned tool parts. ---
    for (_call_id, partial) in accumulator.drain() {
        let desc = if partial.description.is_empty() {
            "(interrupted)".to_string()
        } else {
            format!("{} (interrupted)", partial.description)
        };
        let _ = event_tx
            .send(HarnessEvent::ToolUse {
                tool: partial.tool,
                description: desc,
                input: partial.input,
                output: None,
            })
            .await;
    }

    if sidecar_died {
        // Stream closed or errored in a way that suggests the sidecar is gone.
        // Clear the shared sidecar handle so the NEXT prompt respawns cleanly.
        // Scope the session-map cleanup to JUST this session so a single
        // transport hiccup doesn't nuke every other named session (Fix
        // C2-MEDIUM-6).
        {
            let mut g = sidecar.lock().await;
            *g = None;
        }
        if let Ok(mut s) = sessions.lock() {
            s.retain(|_name, id| id != &session_id);
        }
        let _ = event_tx
            .send(HarnessEvent::Error(
                "opencode sidecar died; session lost, re-run to start fresh".to_string(),
            ))
            .await;
        emit_finish(&ambient_tx, &session_id, "error");
        return;
    }

    // Clean end: emit Done + HarnessFinished.
    let _ = event_tx
        .send(HarnessEvent::Done {
            session_id: session_id.clone(),
        })
        .await;
    emit_finish(&ambient_tx, &session_id, "ok");
}

/// Helper: ensure sidecar from a detached `Arc<Mutex<Option<Sidecar>>>`. The
/// `OpencodeHarness::ensure_sidecar` method requires `&self`, but the spawned
/// task holds handles directly; this mirror keeps error mapping consistent.
async fn ensure_sidecar_via(
    sidecar: &Arc<Mutex<Option<Sidecar>>>,
    config: &OpencodeConfig,
) -> Result<OpencodeClient> {
    let mut guard = sidecar.lock().await;
    if let Some(ref sc) = *guard {
        return Ok(sc.client.clone());
    }

    // If the user pinned a `binary_path`, pre-check it so we can emit a
    // precise error (not-found vs directory vs not-executable vs permission)
    // instead of letting the SDK spawn fail generically (Fix C2-MEDIUM-10).
    if let Some(ref p) = config.binary_path {
        match std::fs::metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(anyhow!("opencode binary at {:?} not found", p));
            }
            Err(e) => {
                return Err(anyhow!("opencode binary at {:?} inaccessible: {}", p, e));
            }
            Ok(m) if m.is_dir() => {
                return Err(anyhow!("opencode binary path {:?} is a directory", p));
            }
            Ok(m) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if m.permissions().mode() & 0o111 == 0 {
                        return Err(anyhow!("opencode binary at {:?} is not executable", p));
                    }
                }
                let _ = m; // silence unused-var warning on non-unix
            }
        }
    }

    let port = config.port.unwrap_or(4096);
    let mut opts = OpencodeServerOptions {
        hostname: "127.0.0.1".to_string(),
        port,
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    if let Some(ref p) = config.binary_path {
        opts.cli_path = Some(p.clone());
    }

    let server = match create_opencode_server(Some(opts)).await {
        Ok(s) => s,
        Err(e) => return Err(map_sidecar_error(e, config, port)),
    };

    let client = create_opencode_client(Some(OpencodeClientConfig {
        base_url: server.url.clone(),
        timeout: Duration::from_secs(60),
        ..Default::default()
    }))
    .map_err(|e| anyhow!("failed to create opencode HTTP client: {}", e))?;

    info!("opencode sidecar started at {}", server.url);
    *guard = Some(Sidecar {
        server,
        client: client.clone(),
    });
    Ok(client)
}

/// Create a fresh session and return its ULID.
async fn create_session(client: &OpencodeClient) -> Result<String> {
    let resp = client
        .session()
        .create(RequestOptions::default().with_body(json!({})))
        .await
        .map_err(|e| anyhow!("session.create failed: {}", e))?;
    let id = resp
        .data
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("session.create response missing `id` field: {}", resp.data))?;
    Ok(id.to_string())
}

/// Translate the SSE stream into `HarnessEvent`s.  Returns via `&mut
/// sidecar_died = true` if the stream errored in a way that strongly suggests
/// the sidecar is gone (transport closure without an idle/completion event).
///
/// `saw_recognized_event` is set true once any `TranslatedEvent::Text` or
/// `TranslatedEvent::ToolUse` is emitted; `saw_unknown_event` is set true
/// when an event with a non-empty name is received but `translate_event`
/// returns `None`. The caller uses these to detect version mismatches when
/// the stream produces nothing user-visible (Fix C2-MAJOR-3).
#[allow(clippy::too_many_arguments)]
async fn process_sse_stream(
    mut sse: SseStream,
    session_id: &str,
    event_tx: &mpsc::Sender<HarnessEvent>,
    accumulator: &mut HashMap<String, PartialToolPart>,
    sidecar_died: &mut bool,
    saw_recognized_event: &mut bool,
    saw_unknown_event: &mut bool,
) {
    // Log non-JSON SSE data at most once per stream to avoid log flooding when
    // a misconfigured sidecar emits malformed frames every tick (Fix C2-MAJOR-3a).
    let mut seen_non_json = false;
    while let Some(next) = sse.next().await {
        let ev = match next {
            Ok(e) => e,
            Err(e) => {
                // Transport-level error before a clean terminal event — the
                // sidecar is likely gone.
                warn!("opencode SSE error: {}", e);
                *sidecar_died = true;
                return;
            }
        };

        let event_name = ev.event.as_deref().unwrap_or("");
        let data: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => {
                if !seen_non_json {
                    warn!(
                        "opencode SSE event {:?} with non-JSON data: {:?} (further non-JSON frames on this stream will be suppressed)",
                        event_name, ev.data
                    );
                    seen_non_json = true;
                } else {
                    debug!(
                        "opencode SSE event {:?} with non-JSON data (suppressed)",
                        event_name
                    );
                }
                continue;
            }
        };

        // Filter to events relevant to our session.  The SDK exposes a global
        // `/event` stream by default, but `/session/{id}/message` is narrower
        // if available.  Scope-match on any `sessionID`/`properties.sessionID`.
        if let Some(sid) = extract_session_id(&data) {
            if sid != session_id {
                continue;
            }
        }

        match translate_event(event_name, &data, accumulator) {
            Some(out_events) => {
                for out in out_events {
                    match out {
                        TranslatedEvent::Text(text) => {
                            *saw_recognized_event = true;
                            let _ = event_tx.send(HarnessEvent::Text(text)).await;
                        }
                        TranslatedEvent::ToolUse {
                            tool,
                            description,
                            input,
                            output,
                        } => {
                            *saw_recognized_event = true;
                            let _ = event_tx
                                .send(HarnessEvent::ToolUse {
                                    tool,
                                    description,
                                    input,
                                    output,
                                })
                                .await;
                        }
                        TranslatedEvent::SessionIdle => {
                            // Clean terminal event — not a sidecar death.
                            return;
                        }
                    }
                }
            }
            None => {
                // `translate_event` returned None: either the event had an
                // empty name (nothing useful) or the name wasn't one we
                // recognise. Flag the latter so the caller can surface a
                // version-mismatch hint if nothing recognised ever arrives.
                if !event_name.is_empty() {
                    *saw_unknown_event = true;
                }
            }
        }
    }

    // Natural stream end without an explicit idle/complete event.  opencode
    // closes `/event` on idle too, so we do NOT mark this as sidecar death —
    // the caller's orphan-drain handles any partials.
}

/// Extract the `sessionID` (or nested `properties.sessionID`) from an event
/// payload, if present. Helps when the global `/event` stream mixes sessions.
fn extract_session_id(data: &Value) -> Option<&str> {
    data.get("sessionID")
        .and_then(|v| v.as_str())
        .or_else(|| {
            data.get("properties")
                .and_then(|p| p.get("sessionID"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            data.get("properties")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("sessionID"))
                .and_then(|v| v.as_str())
        })
}

/// Intermediate result of `translate_event`. Decouples pure translation from
/// the async emission so the logic is unit-testable.
#[derive(Debug, Clone)]
enum TranslatedEvent {
    Text(String),
    ToolUse {
        tool: String,
        description: String,
        input: Option<String>,
        output: Option<String>,
    },
    SessionIdle,
}

/// Pure event translation. Returns zero or more `TranslatedEvent`s AND
/// mutates the accumulator. Extracted so tests can exercise accumulator
/// bookkeeping without a live sidecar.
fn translate_event(
    event_name: &str,
    data: &Value,
    accumulator: &mut HashMap<String, PartialToolPart>,
) -> Option<Vec<TranslatedEvent>> {
    match event_name {
        // Text delta frames.  Field names mirror the JS SDK surface — we
        // probe a few plausible shapes because the wire format has drifted.
        "message.part.delta" | "message.part.updated" => {
            // Text branch: check for a text payload first.
            if let Some(text) = extract_text_delta(data) {
                if !text.is_empty() {
                    return Some(vec![TranslatedEvent::Text(text)]);
                }
            }

            // Tool-part branch: look for a `part` with `type == "tool"` and a
            // `state` object keyed by `callID` (or similar).
            if let Some(tool) = extract_tool_part(data) {
                return translate_tool_part(tool, accumulator);
            }

            None
        }
        "session.idle" | "session.complete" | "message.end" => {
            Some(vec![TranslatedEvent::SessionIdle])
        }
        _ => None,
    }
}

/// Pull a text delta out of a `message.part.*` payload.  Handles both
/// `{ part: { type: "text", text: "..." } }` and
/// `{ properties: { part: { ... } } }` shapes.
fn extract_text_delta(data: &Value) -> Option<String> {
    let part = data
        .get("part")
        .or_else(|| data.get("properties").and_then(|p| p.get("part")))?;
    let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ptype != "text" {
        return None;
    }
    part.get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// Pull the tool part object out of a `message.part.*` payload, if the
/// payload is for a tool part.
fn extract_tool_part(data: &Value) -> Option<&Value> {
    let part = data
        .get("part")
        .or_else(|| data.get("properties").and_then(|p| p.get("part")))?;
    let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ptype != "tool" {
        return None;
    }
    Some(part)
}

/// Handle the state machine for a tool part: accumulate on `running`, emit
/// once on `completed` / `error`, do nothing on `pending`.
fn translate_tool_part(
    part: &Value,
    accumulator: &mut HashMap<String, PartialToolPart>,
) -> Option<Vec<TranslatedEvent>> {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let tool_name = part
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = part.get("state")?;
    let status = state
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");

    match status {
        "pending" => None,
        "running" => {
            let input = state
                .get("input")
                .map(|v| truncate_json(v, 4096))
                .or_else(|| {
                    state
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                });
            let entry = accumulator.entry(call_id.clone()).or_default();
            entry.tool = tool_name;
            entry.input = input.clone();
            entry.description = describe_tool_input(&entry.tool, state);
            None
        }
        "completed" => {
            let partial = accumulator.remove(&call_id).unwrap_or_default();
            let tool = if partial.tool.is_empty() {
                tool_name
            } else {
                partial.tool
            };
            let input = partial
                .input
                .or_else(|| state.get("input").map(|v| truncate_json(v, 4096)));
            let output = state
                .get("output")
                .map(|v| truncate_json(v, 4096))
                .or_else(|| state.get("result").map(|v| truncate_json(v, 4096)));
            let desc = if partial.description.is_empty() {
                describe_tool_input(&tool, state)
            } else {
                partial.description
            };
            Some(vec![TranslatedEvent::ToolUse {
                tool,
                description: desc,
                input,
                output,
            }])
        }
        "error" | "failed" => {
            let partial = accumulator.remove(&call_id).unwrap_or_default();
            let tool = if partial.tool.is_empty() {
                tool_name
            } else {
                partial.tool
            };
            let err_msg = state
                .get("error")
                .and_then(|v| v.as_str())
                .or_else(|| state.get("message").and_then(|v| v.as_str()))
                .unwrap_or("error");
            let desc = if partial.description.is_empty() {
                format!("error: {}", err_msg)
            } else {
                format!("{} — error: {}", partial.description, err_msg)
            };
            Some(vec![TranslatedEvent::ToolUse {
                tool,
                description: desc,
                input: partial.input,
                output: None,
            }])
        }
        _ => None,
    }
}

/// Render a tool input field into a short, human-readable description.
fn describe_tool_input(tool: &str, state: &Value) -> String {
    let input = state.get("input");
    if let Some(input) = input {
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            return super::truncate(cmd, 80);
        }
        if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
            return path.to_string();
        }
        if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
            return super::truncate(pattern, 80);
        }
        if !tool.is_empty() {
            return super::truncate(&input.to_string(), 100);
        }
    }
    if let Some(title) = state.get("title").and_then(|t| t.as_str()) {
        return super::truncate(title, 80);
    }
    String::new()
}

fn truncate_json(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    super::truncate(&s, max)
}

fn emit_started(ambient_tx: &Option<broadcast::Sender<AmbientEvent>>, session_id: &str) {
    if let Some(ref tx) = ambient_tx {
        let _ = tx.send(AmbientEvent::HarnessStarted {
            harness: HarnessKind::Opencode.name().to_string(),
            run_id: session_id.to_string(),
            prompt_hash: None,
        });
    }
}

fn emit_finish(ambient_tx: &Option<broadcast::Sender<AmbientEvent>>, run_id: &str, status: &str) {
    if let Some(ref tx) = ambient_tx {
        let _ = tx.send(AmbientEvent::HarnessFinished {
            harness: HarnessKind::Opencode.name().to_string(),
            run_id: run_id.to_string(),
            status: status.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn running_event(call_id: &str, tool: &str, cmd: &str) -> Value {
        json!({
            "sessionID": "sess_1",
            "part": {
                "type": "tool",
                "callID": call_id,
                "tool": tool,
                "state": {
                    "status": "running",
                    "input": { "command": cmd },
                }
            }
        })
    }

    fn completed_event(call_id: &str, tool: &str, cmd: &str, output: &str) -> Value {
        json!({
            "sessionID": "sess_1",
            "part": {
                "type": "tool",
                "callID": call_id,
                "tool": tool,
                "state": {
                    "status": "completed",
                    "input": { "command": cmd },
                    "output": output,
                }
            }
        })
    }

    fn error_event(call_id: &str, tool: &str, cmd: &str, err: &str) -> Value {
        json!({
            "sessionID": "sess_1",
            "part": {
                "type": "tool",
                "callID": call_id,
                "tool": tool,
                "state": {
                    "status": "error",
                    "input": { "command": cmd },
                    "error": err,
                }
            }
        })
    }

    fn text_event(text: &str) -> Value {
        json!({
            "sessionID": "sess_1",
            "part": { "type": "text", "text": text }
        })
    }

    #[test]
    fn accumulator_running_then_completed_emits_once() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();

        // Running: should accumulate, emit nothing.
        let out = translate_event(
            "message.part.updated",
            &running_event("c1", "Bash", "ls"),
            &mut acc,
        );
        assert!(out.is_none() || out.as_ref().map(|v| v.is_empty()).unwrap_or(true));
        assert_eq!(acc.len(), 1, "running should accumulate one entry");

        // Completed: should emit one ToolUse with input AND output set.
        let out = translate_event(
            "message.part.updated",
            &completed_event("c1", "Bash", "ls", "total 42"),
            &mut acc,
        )
        .expect("completed must emit");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => {
                assert_eq!(tool, "Bash");
                assert!(input.as_deref().unwrap_or("").contains("ls"));
                assert!(output.as_deref().unwrap_or("").contains("total 42"));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert!(acc.is_empty(), "completed must drain accumulator");
    }

    #[test]
    fn accumulator_running_then_error_emits_with_no_output() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();

        let _ = translate_event(
            "message.part.updated",
            &running_event("c2", "Read", "/x"),
            &mut acc,
        );
        let out = translate_event(
            "message.part.updated",
            &error_event("c2", "Read", "/x", "permission denied"),
            &mut acc,
        )
        .expect("error must emit");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                description,
                output,
                ..
            } => {
                assert_eq!(tool, "Read");
                assert!(description.contains("permission denied"));
                assert!(output.is_none(), "error should have no output");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert!(acc.is_empty());
    }

    #[test]
    fn text_delta_emits_text_event() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();
        let out = translate_event("message.part.updated", &text_event("hello"), &mut acc)
            .expect("text must emit");
        assert_eq!(out.len(), 1);
        match &out[0] {
            TranslatedEvent::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn pending_state_is_ignored() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();
        let ev = json!({
            "sessionID": "sess_1",
            "part": {
                "type": "tool",
                "callID": "c3",
                "tool": "Bash",
                "state": { "status": "pending" }
            }
        });
        let out = translate_event("message.part.updated", &ev, &mut acc);
        assert!(out.is_none() || out.as_ref().unwrap().is_empty());
        assert!(acc.is_empty());
    }

    #[test]
    fn session_idle_emits_terminal_event() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();
        let out = translate_event("session.idle", &json!({}), &mut acc).expect("idle emits");
        assert!(matches!(out[0], TranslatedEvent::SessionIdle));
    }

    #[test]
    fn orphan_drain_emits_one_per_entry() {
        // Simulate a Running with no matching Completed, then drain.
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();
        let _ = translate_event(
            "message.part.updated",
            &running_event("c4", "Grep", "foo"),
            &mut acc,
        );
        assert_eq!(acc.len(), 1);

        // Drain inline (mirrors the logic in run_opencode_prompt_inner).
        let mut drained: Vec<TranslatedEvent> = Vec::new();
        for (_call_id, partial) in acc.drain() {
            drained.push(TranslatedEvent::ToolUse {
                tool: partial.tool,
                description: format!("{} (interrupted)", partial.description),
                input: partial.input,
                output: None,
            });
        }
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            TranslatedEvent::ToolUse {
                tool,
                description,
                output,
                ..
            } => {
                assert_eq!(tool, "Grep");
                assert!(description.contains("interrupted"));
                assert!(output.is_none());
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn unknown_event_names_are_ignored() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();
        let out = translate_event("something.weird", &json!({"x": 1}), &mut acc);
        assert!(out.is_none());
        assert!(acc.is_empty());
    }

    #[test]
    fn harness_kind_is_opencode() {
        let h = OpencodeHarness::new_with_config(OpencodeConfig::default());
        assert_eq!(h.kind(), HarnessKind::Opencode);
        assert!(h.supports_resume());
    }

    #[test]
    fn get_set_session_id_roundtrips() {
        let h = OpencodeHarness::new_with_config(OpencodeConfig::default());
        assert_eq!(h.get_session_id("foo"), None);
        h.set_session_id("foo", "sess_123".to_string());
        assert_eq!(h.get_session_id("foo"), Some("sess_123".to_string()));
    }

    // NOTE: This test covers the broadcast-channel plumbing only (the
    // `emit_finish` helper + direct `tx.send(HarnessStarted)`). End-to-end
    // `run_prompt` → `HarnessStarted` / `HarnessFinished` coverage requires a
    // live sidecar and is the gated integration test's responsibility.
    #[test]
    fn ambient_events_emit_helpers_roundtrip_through_broadcast() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(16);
        let session_id = "sess_test_ambient";

        // Simulate the HarnessStarted emission from run_opencode_prompt_inner.
        let _ = tx.send(AmbientEvent::HarnessStarted {
            harness: HarnessKind::Opencode.name().to_string(),
            run_id: session_id.to_string(),
            prompt_hash: None,
        });

        // Simulate the HarnessFinished emission via emit_finish.
        emit_finish(&Some(tx.clone()), session_id, "ok");

        // Both events must have arrived on the subscriber.
        let ev1 = rx.try_recv().expect("HarnessStarted must be received");
        match ev1 {
            AmbientEvent::HarnessStarted {
                ref harness,
                ref run_id,
                ..
            } => {
                assert_eq!(harness, "OpenCode");
                assert_eq!(run_id, session_id);
            }
            other => panic!("expected HarnessStarted, got {:?}", other),
        }

        let ev2 = rx.try_recv().expect("HarnessFinished must be received");
        match ev2 {
            AmbientEvent::HarnessFinished {
                ref harness,
                ref run_id,
                ref status,
            } => {
                assert_eq!(harness, "OpenCode");
                assert_eq!(run_id, session_id);
                assert_eq!(status, "ok");
            }
            other => panic!("expected HarnessFinished, got {:?}", other),
        }

        // No more events.
        assert!(rx.try_recv().is_err(), "no further events expected");
    }

    /// Pins the `AmbientEvent::HarnessStarted.harness` payload shape: it
    /// must be exactly `"OpenCode"` (the value of `HarnessKind::Opencode.name()`).
    /// A silent rename of the enum variant or its `name()` mapping would
    /// break socket subscribers; this test is the regression guard.
    #[test]
    fn harness_started_payload_matches_kind() {
        let ev = AmbientEvent::HarnessStarted {
            harness: HarnessKind::Opencode.name().to_string(),
            run_id: "x".into(),
            prompt_hash: None,
        };
        match ev {
            AmbientEvent::HarnessStarted { harness, .. } => {
                assert_eq!(harness, "OpenCode");
            }
            other => panic!("expected HarnessStarted, got {:?}", other),
        }
    }

    /// Exercise the `emit_started` helper that the production `run_prompt`
    /// path calls. Subscribing to a real broadcast channel and asserting
    /// receipt pins the production emission site behavior without needing
    /// a live sidecar.
    #[test]
    fn emit_started_delivers_harness_started_through_broadcast() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(8);
        emit_started(&Some(tx), "sess_started");
        let ev = rx.try_recv().expect("HarnessStarted must arrive");
        match ev {
            AmbientEvent::HarnessStarted {
                harness,
                run_id,
                prompt_hash,
            } => {
                assert_eq!(harness, "OpenCode");
                assert_eq!(run_id, "sess_started");
                assert!(prompt_hash.is_none());
            }
            other => panic!("expected HarnessStarted, got {:?}", other),
        }
    }

    /// `emit_started` must be a no-op when the ambient bus is absent.
    /// Paired with a positive control to prove the channel works — verifies
    /// that None emits nothing and Some emits exactly one event.
    #[test]
    fn emit_started_with_no_bus_is_noop() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(4);
        // None bus: must NOT send anything.
        emit_started(&None, "sess_noop");
        // Some bus: must send exactly one event, proving the channel works.
        emit_started(&Some(tx), "sess_with_bus");
        let ev = rx
            .try_recv()
            .expect("Some-bus call must deliver exactly one event");
        match ev {
            AmbientEvent::HarnessStarted { ref run_id, .. } => {
                assert_eq!(run_id, "sess_with_bus");
            }
            other => panic!("wrong variant: {:?}", other),
        }
        assert!(
            rx.try_recv().is_err(),
            "None-bus call must not have emitted anything"
        );
    }

    // ─── map_sidecar_error coverage (Fix C3-HIGH-1) ──────────────────────────
    //
    // These pin the chat-surfaced error text for each recognised `SdkError`
    // shape so a regression in the SDK (variant rename, field change) or in
    // our mapper is caught before it ships to users.

    #[test]
    fn map_error_startup_timeout_emits_5s_message() {
        let err = SdkError::ServerStartupTimeout { timeout_ms: 5000 };
        let mapped = map_sidecar_error(err, &OpencodeConfig::default(), 4096);
        let msg = format!("{:#}", mapped);
        assert_eq!(msg, "opencode sidecar did not start within 5s");
    }

    #[test]
    fn map_error_cli_not_found_without_binary_path_says_not_on_path() {
        use opencode::errors::CLINotFoundError;
        let err = SdkError::CLINotFound(CLINotFoundError::new("not found", None));
        let cfg = OpencodeConfig::default();
        let mapped = map_sidecar_error(err, &cfg, 4096);
        let msg = format!("{:#}", mapped);
        assert_eq!(msg, "opencode not on PATH");
    }

    #[test]
    fn map_error_cli_not_found_with_binary_path_includes_path() {
        use opencode::errors::CLINotFoundError;
        let err = SdkError::CLINotFound(CLINotFoundError::new(
            "not found",
            Some("/custom/bin".to_string()),
        ));
        let cfg = OpencodeConfig {
            binary_path: Some(std::path::PathBuf::from("/custom/bin")),
            ..Default::default()
        };
        let mapped = map_sidecar_error(err, &cfg, 4096);
        let msg = format!("{:#}", mapped);
        // Message contains a dynamic SDK error field so exact-match isn't
        // possible, but both key substrings must appear together.
        assert!(
            msg.contains("/custom/bin"),
            "expected binary path in message, got: {}",
            msg
        );
        assert!(
            msg.contains("not executable"),
            "expected 'not executable' phrasing, got: {}",
            msg
        );
    }

    #[test]
    fn map_error_io_addr_in_use_includes_port() {
        let err = SdkError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "addr in use",
        ));
        let mapped = map_sidecar_error(err, &OpencodeConfig::default(), 4099);
        let msg = format!("{:#}", mapped);
        assert_eq!(
            msg,
            format!(
                "127.0.0.1:{} in use, check for another opencode instance",
                4099
            )
        );
    }

    #[test]
    fn map_error_io_not_found_without_binary_path_says_not_on_path() {
        let err = SdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ));
        let cfg = OpencodeConfig::default();
        let mapped = map_sidecar_error(err, &cfg, 4096);
        let msg = format!("{:#}", mapped);
        assert_eq!(msg, "opencode not on PATH");
    }

    // ─── accumulator interleaving (Fix C3-HIGH-2) ───────────────────────────

    /// Two concurrent tool calls with distinct `callID`s must be tracked
    /// independently: completing one must not affect or leak into the other.
    #[test]
    fn accumulator_handles_interleaved_callids_independently() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();

        // Running c1 and c2 — no emissions, two accumulator entries.
        let out1 = translate_event(
            "message.part.updated",
            &running_event("c1", "Bash", "ls"),
            &mut acc,
        );
        assert!(out1.is_none() || out1.as_ref().unwrap().is_empty());
        let out2 = translate_event(
            "message.part.updated",
            &running_event("c2", "Read", "/etc/hosts"),
            &mut acc,
        );
        assert!(out2.is_none() || out2.as_ref().unwrap().is_empty());
        assert_eq!(acc.len(), 2, "two running events must produce two entries");

        // Complete c1 — 1 emission, c2 remains.
        let out = translate_event(
            "message.part.updated",
            &completed_event("c1", "Bash", "ls", "one.txt"),
            &mut acc,
        )
        .expect("completed c1 must emit");
        assert_eq!(out.len(), 1);
        let (tool1, input1, output1) = match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => (tool.clone(), input.clone(), output.clone()),
            other => panic!("expected ToolUse, got {:?}", other),
        };
        assert_eq!(tool1, "Bash");
        assert!(input1.as_deref().unwrap_or("").contains("ls"));
        assert!(output1.as_deref().unwrap_or("").contains("one.txt"));
        assert_eq!(acc.len(), 1, "c2 must still be in the accumulator");

        // Complete c2 — 1 emission, accumulator empty.
        let out = translate_event(
            "message.part.updated",
            &completed_event("c2", "Read", "/etc/hosts", "127.0.0.1 localhost"),
            &mut acc,
        )
        .expect("completed c2 must emit");
        assert_eq!(out.len(), 1);
        let (tool2, input2, output2) = match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => (tool.clone(), input.clone(), output.clone()),
            other => panic!("expected ToolUse, got {:?}", other),
        };
        assert_eq!(tool2, "Read");
        assert!(input2.as_deref().unwrap_or("").contains("/etc/hosts"));
        assert!(output2.as_deref().unwrap_or("").contains("127.0.0.1"));
        assert!(
            acc.is_empty(),
            "accumulator must drain after last completed"
        );

        // Cross-talk guard — the two emissions must be genuinely distinct.
        assert_ne!(tool1, tool2);
        assert_ne!(input1, input2);
        assert_ne!(output1, output2);
    }

    // ─── duplicate-completed idempotency (Fix C3-HIGH-3) ────────────────────

    /// A second `completed` event for the same `callID` after the first has
    /// already drained the accumulator is a degenerate shape. The current
    /// behavior emits a ToolUse with an empty tool + the state's own
    /// input/output copied through (because `remove` returns `None` →
    /// `PartialToolPart::default()` is used, and the tool-name fallback
    /// reads from the part payload).
    ///
    /// This test pins that behavior so a change is a conscious choice, not a
    /// silent regression.
    /// TODO: consider short-circuiting to `None` if the accumulator has no
    /// entry for the callID — would be cleaner for downstream consumers.
    #[test]
    fn accumulator_second_completed_for_same_callid_is_stateless_emit() {
        let mut acc: HashMap<String, PartialToolPart> = HashMap::new();

        // Running → Completed: drains the entry.
        let _ = translate_event(
            "message.part.updated",
            &running_event("c1", "Bash", "ls"),
            &mut acc,
        );
        let _ = translate_event(
            "message.part.updated",
            &completed_event("c1", "Bash", "ls", "total 42"),
            &mut acc,
        );
        assert!(acc.is_empty(), "first completed drains the accumulator");

        // Second completed with the same callID: accumulator has no entry;
        // `remove` returns None → PartialToolPart::default() is used. The
        // tool-name fallback reads from the event payload ("Bash"), and
        // input/output are copied directly from the event state. Current
        // behavior is `Some(vec![ToolUse { tool: "Bash", input: Some(_),
        // output: Some(_) }])`. Pin this hard so a silent behavior change
        // (e.g. short-circuiting to None) is caught.
        // TODO: consider short-circuiting to `None` if the accumulator has no
        // entry for the callID — would be cleaner for downstream consumers.
        let out = translate_event(
            "message.part.updated",
            &completed_event("c1", "Bash", "ls", "total 42"),
            &mut acc,
        )
        .expect("second completed must emit (current stateless-emit behavior)");
        assert_eq!(out.len(), 1, "must emit exactly one ToolUse");
        match &out[0] {
            TranslatedEvent::ToolUse {
                tool,
                input,
                output,
                ..
            } => {
                // Tool name falls back to the event payload's value.
                assert_eq!(tool, "Bash");
                // Input and output are copied from the event state.
                assert!(
                    input.as_deref().unwrap_or("").contains("ls"),
                    "input must contain the command"
                );
                assert!(
                    output.as_deref().unwrap_or("").contains("total 42"),
                    "output must contain the result"
                );
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert!(
            acc.is_empty(),
            "second completed must leave accumulator empty"
        );
    }

    // ─── Arc<OpencodeHarness> forwarding (Fix C3-MEDIUM-6) ──────────────────

    /// The `Harness` impl on `Arc<OpencodeHarness>` must delegate all trait
    /// methods to the inner harness, and both sides must observe the same
    /// backing state (session map, kind, resume support).
    #[test]
    fn arc_forwarder_delegates_all_trait_methods() {
        let inner = OpencodeHarness::new_with_config(OpencodeConfig::default());
        let inner_supports_resume = inner.supports_resume();
        let h: Arc<OpencodeHarness> = Arc::new(inner);

        // kind() forwards.
        assert_eq!(
            <Arc<OpencodeHarness> as Harness>::kind(&h),
            HarnessKind::Opencode
        );
        // supports_resume forwards.
        assert_eq!(
            <Arc<OpencodeHarness> as Harness>::supports_resume(&h),
            inner_supports_resume
        );

        // set via Arc → read via inner.
        <Arc<OpencodeHarness> as Harness>::set_session_id(&h, "alpha", "sess_alpha".to_string());
        assert_eq!(
            (*h).get_session_id("alpha"),
            Some("sess_alpha".to_string()),
            "Arc set must be visible via the inner harness"
        );

        // set via inner → read via Arc.
        (*h).set_session_id("beta", "sess_beta".to_string());
        assert_eq!(
            <Arc<OpencodeHarness> as Harness>::get_session_id(&h, "beta"),
            Some("sess_beta".to_string()),
            "inner set must be visible via the Arc"
        );
    }

    // ─── panic-safety (Fix C3-MEDIUM-5) ─────────────────────────────────────

    #[test]
    fn format_panic_message_str_payload() {
        let msg = format_panic_message(Box::new("boom"));
        assert_eq!(msg, "opencode: internal panic: boom");
    }

    #[test]
    fn format_panic_message_string_payload() {
        let msg = format_panic_message(Box::new(String::from("kaboom")));
        assert_eq!(msg, "opencode: internal panic: kaboom");
    }

    #[test]
    fn format_panic_message_unknown_payload() {
        // A non-string payload must fall through to the generic message.
        let msg = format_panic_message(Box::new(42_i32));
        assert_eq!(msg, "opencode: internal panic (unknown)");
    }

    #[test]
    fn ambient_events_harness_finished_with_error_status() {
        let (tx, mut rx) = broadcast::channel::<AmbientEvent>(8);
        emit_finish(&Some(tx), "sess_err", "error");

        let ev = rx.try_recv().expect("event must arrive");
        match ev {
            AmbientEvent::HarnessFinished { status, .. } => {
                assert_eq!(status, "error");
            }
            other => panic!("expected HarnessFinished, got {:?}", other),
        }
    }

    #[test]
    fn ambient_none_bus_does_not_panic() {
        // Calling emit_finish with no ambient bus should be a no-op.
        emit_finish(&None, "sess_none", "ok");
    }
}
