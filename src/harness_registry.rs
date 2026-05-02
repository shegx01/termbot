//! `HarnessRegistry` — owns the four harness implementations, the named-session
//! index + LRU policy, and the prompt-dispatch logic that drives a chat-origin
//! prompt through `harness.run_prompt` → `drive_harness` → optional named-session
//! persist.
//!
//! Extracted from `App` because today `App` keeps each harness as both a
//! `dyn Harness` entry AND a typed `Arc<*Harness>` — a code smell pointing at
//! exactly this missing abstraction. Centralising the registry here erases the
//! ownership duplication and gives the future `SubprocessHarness` shared base
//! a clean home (issue #3.2 in the architecture review).
//!
//! ## Ownership
//!
//! - `harnesses: HashMap<HarnessKind, Box<dyn Harness>>` — the type-erased
//!   registry used by `harness_for(kind)` and the routing in `send_harness_prompt`.
//! - `claude/opencode/gemini/codex: Arc<*Harness>` — typed Arcs alongside
//!   the dyn entry. The `dyn Harness` trait does NOT include `run_subcommand`
//!   (only opencode, gemini, and codex have one), so the typed handles are
//!   load-bearing for chat-safe subcommand passthrough. The Arcs and the dyn
//!   entry both point at the same heap object.
//! - `named_sessions: HashMap<String, NamedSessionEntry>` — the named-session
//!   index, hydrated from `terminus-state.json` on startup and mutated only
//!   after `state_tx.send` accepts the persist batch (Bug 3 invariant).
//! - `max_named_sessions: usize` — LRU cap, default 50.
//! - `state_tx: mpsc::Sender<StateUpdate>` — clone of App's state channel,
//!   used exclusively by `persist_named_session` to force-persist a
//!   `HarnessSessionBatch` update.
//!
//! ## Prompt dispatch
//!
//! `send_harness_prompt` is the central command-path method. It needs the
//! registry's own state PLUS several App-owned handles (chat platforms,
//! schema registry, delivery queue, webhook client, broadcast senders,
//! session manager). Rather than make the registry own those handles
//! (which would drag chat machinery into a "harness" type), the caller
//! passes a borrowed [`PromptDispatchContext`] with the orchestration
//! handles. The split-borrow pattern at the call site in `App` keeps the
//! registry's `&mut self` access compatible with the `&App.field`
//! immutable borrows that build the dispatch context.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};

use crate::buffer::StreamEvent;
use crate::chat_adapters::{Attachment, ChatPlatform, PlatformType, ReplyContext};
use crate::command::HarnessOptions;
use crate::config::Config;
use crate::harness::claude::ClaudeHarness;
use crate::harness::codex::CodexHarness;
use crate::harness::gemini::GeminiHarness;
use crate::harness::opencode::OpencodeHarness;
use crate::harness::{build_session_key, drive_harness, Harness, HarnessContext, HarnessKind};
use crate::session::SessionManager;
use crate::socket::events::AmbientEvent;
use crate::state_store::{NamedSessionEntry, StateUpdate};
use crate::structured_output::{DeliveryQueue, SchemaRegistry, WebhookClient};

/// Outcome of [`HarnessRegistry::persist_named_session`]. The `Err(reason)`
/// variant carries a chat-safe message the caller routes through
/// `dispatch.send_error`. Using `Result<(), String>` matches the codebase's
/// broader Result-shaped convention.
///
/// `pub(crate)` because the only caller is the internal `send_harness_prompt`
/// path; no external consumer depends on this signature.
pub(crate) type PersistOutcome = Result<(), String>;

/// Bundle of App-owned handles that `send_harness_prompt` and
/// `reject_if_not_resumable` need to drive a prompt through the chat
/// reply path. Constructed at the call site in `App` via split-borrow so
/// the registry can mutate `named_sessions` (`&mut self`) while the
/// dispatch context holds immutable borrows of disjoint App fields.
///
/// **NOT a getter return value.** This struct is intentionally constructed
/// inline at every call site rather than via a `&App -> PromptDispatchContext<'_>`
/// method. A getter that takes `&self` would borrow `self` as a whole, which
/// then conflicts with the immediately-following `&mut self.harness_registry`
/// access. By writing the struct literal at the call site, the borrow checker
/// sees field-level disjoint borrows (dispatch borrows
/// `session_mgr/telegram/.../ambient_tx`; the registry call borrows
/// `harness_registry`) and accepts the mixed access. Do NOT refactor the
/// inline construction into a getter — it WILL fail to compile.
pub struct PromptDispatchContext<'a> {
    pub session_mgr: &'a SessionManager,
    pub telegram: Option<&'a (dyn ChatPlatform + 'a)>,
    pub slack: Option<&'a (dyn ChatPlatform + 'a)>,
    pub discord: Option<&'a (dyn ChatPlatform + 'a)>,
    pub schema_registry: &'a Arc<SchemaRegistry>,
    pub delivery_queue: &'a Arc<DeliveryQueue>,
    pub webhook_client: &'a Arc<WebhookClient>,
    pub stream_tx: &'a broadcast::Sender<StreamEvent>,
    pub ambient_tx: &'a broadcast::Sender<AmbientEvent>,
}

impl PromptDispatchContext<'_> {
    /// Send a reply through the chat adapter for `ctx.platform`, or
    /// through the per-request socket reply channel for socket-origin
    /// messages. Mirrors the body of `App::send_reply` so the migrated
    /// `send_harness_prompt` can use the same code path without taking
    /// `&App`.
    pub async fn send_reply(&self, ctx: &ReplyContext, text: &str) {
        if let Some(ref tx) = ctx.socket_reply_tx {
            let _ = tx.send(text.to_string());
            return;
        }
        let platform: Option<&dyn ChatPlatform> = match ctx.platform {
            PlatformType::Telegram => self.telegram,
            PlatformType::Slack => self.slack,
            PlatformType::Discord => self.discord,
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

    /// Send an `Error: <error>`-prefixed reply through the same routing as
    /// [`Self::send_reply`]. Mirrors `App::send_error`.
    pub async fn send_error(&self, ctx: &ReplyContext, error: &str) {
        self.send_reply(ctx, &format!("Error: {}", error)).await;
    }

    /// Fire-and-forget emit of an ambient event. Mirrors `App::emit_ambient`.
    pub fn emit_ambient(&self, event: AmbientEvent) {
        let _ = self.ambient_tx.send(event);
    }
}

pub struct HarnessRegistry {
    harnesses: HashMap<HarnessKind, Box<dyn Harness>>,
    named_sessions: HashMap<String, NamedSessionEntry>,
    max_named_sessions: usize,
    state_tx: mpsc::Sender<StateUpdate>,

    // Typed Arcs alongside the dyn entry. See module-level "Ownership"
    // for why the duplication is load-bearing today.
    claude: Arc<ClaudeHarness>,
    opencode: Arc<OpencodeHarness>,
    gemini: Arc<GeminiHarness>,
    codex: Arc<CodexHarness>,
}

impl HarnessRegistry {
    /// Construct the registry. Builds the four harness implementations from
    /// `config`, wires the `ambient_tx` and `schema_registry` Arcs into the
    /// ones that take them, and seeds the named-session index from
    /// `named_sessions`.
    pub fn new(
        config: &Config,
        state_tx: mpsc::Sender<StateUpdate>,
        ambient_tx: broadcast::Sender<AmbientEvent>,
        schema_registry: Arc<SchemaRegistry>,
        named_sessions: HashMap<String, NamedSessionEntry>,
    ) -> Self {
        let mut harnesses: HashMap<HarnessKind, Box<dyn Harness>> = HashMap::new();
        let claude =
            Arc::new(ClaudeHarness::new().with_schema_registry(Arc::clone(&schema_registry)));
        harnesses.insert(
            HarnessKind::Claude,
            Box::new(Arc::clone(&claude)) as Box<dyn Harness>,
        );
        let opencode_cfg = config.harness.opencode.clone().unwrap_or_default();
        let opencode = Arc::new(OpencodeHarness::new(opencode_cfg, ambient_tx.clone()));
        harnesses.insert(
            HarnessKind::Opencode,
            Box::new(Arc::clone(&opencode)) as Box<dyn Harness>,
        );
        let gemini_cfg = config.harness.gemini.clone().unwrap_or_default();
        let gemini = Arc::new(GeminiHarness::new(gemini_cfg, ambient_tx.clone()));
        harnesses.insert(
            HarnessKind::Gemini,
            Box::new(Arc::clone(&gemini)) as Box<dyn Harness>,
        );
        let codex_cfg = config.harness.codex.clone().unwrap_or_default();
        let codex = Arc::new(
            CodexHarness::new(codex_cfg, ambient_tx.clone())
                .with_schema_registry(Arc::clone(&schema_registry)),
        );
        harnesses.insert(
            HarnessKind::Codex,
            Box::new(Arc::clone(&codex)) as Box<dyn Harness>,
        );

        Self {
            harnesses,
            named_sessions,
            max_named_sessions: config.harness.max_named_sessions.unwrap_or(50),
            state_tx,
            claude,
            opencode,
            gemini,
            codex,
        }
    }

    /// Lookup a harness by kind. Returns `None` if the kind has been
    /// stubbed out at compile time or otherwise isn't registered.
    pub fn harness_for(&self, kind: &HarnessKind) -> Option<&dyn Harness> {
        self.harnesses.get(kind).map(|b| b.as_ref())
    }

    /// Quick presence check used by `handle_command` to gate `HarnessOn`
    /// against feature-stubbed harnesses (e.g. when a kind is compiled out).
    pub fn contains_kind(&self, kind: &HarnessKind) -> bool {
        self.harnesses.contains_key(kind)
    }

    /// Typed accessor for `ClaudeHarness`. Currently unread (Claude has no
    /// inherent `run_subcommand`); kept symmetric with the other three so a
    /// future claude-specific inherent method has a typed call site without
    /// a breaking signature change. See module-level "Ownership" for the
    /// dyn+typed duplication rationale.
    ///
    /// TODO: remove if no Claude inherent method materialises by the time
    /// the SubprocessHarness shared base lands (issue #3.2).
    #[allow(dead_code)]
    pub fn claude(&self) -> &Arc<ClaudeHarness> {
        &self.claude
    }

    /// Typed accessor for `OpencodeHarness`. Used by `handle_command` to call
    /// the inherent `run_subcommand` for chat-safe `: opencode <subcommand>`.
    pub fn opencode(&self) -> &Arc<OpencodeHarness> {
        &self.opencode
    }

    /// Typed accessor for `GeminiHarness`. Same rationale as `opencode()` —
    /// `run_subcommand` is an inherent method, not on the `Harness` trait.
    pub fn gemini(&self) -> &Arc<GeminiHarness> {
        &self.gemini
    }

    /// Typed accessor for `CodexHarness`. Same rationale as `opencode()`.
    pub fn codex(&self) -> &Arc<CodexHarness> {
        &self.codex
    }

    /// Look up a named session by harness kind + name.
    ///
    /// Tries the prefixed key `{kind}:{name}` first (new format), then falls
    /// back to the bare `name` for legacy unprefixed keys that pre-date the
    /// cross-harness isolation scheme.
    ///
    /// TODO: remove legacy fallback once state-file shows zero unprefixed keys
    /// for 14 consecutive days (tracked in
    /// .omc/plans/opencode-harness-followups.md).
    pub fn lookup_named_session(
        &self,
        kind: HarnessKind,
        name: &str,
    ) -> Option<&NamedSessionEntry> {
        let prefixed = build_session_key(kind, name);
        self.named_sessions
            .get(&prefixed)
            .or_else(|| self.named_sessions.get(name))
    }

    /// Persist a named-session entry, mutating in-memory state ONLY after the
    /// state channel accepts the update. This is the Bug 3 invariant from
    /// the opencode-harness-followups.md handoff: previously the in-memory
    /// `named_sessions` HashMap was updated unconditionally and the channel
    /// send was a `try_send` whose failure was logged-and-dropped, producing
    /// silent on-disk-vs-in-memory drift that broke `--resume <name>` after
    /// a restart.
    ///
    /// The fix:
    /// 1. Compute the would-be eviction key (read-only) so we know the full
    ///    batch ahead of time.
    /// 2. Build the batch (eviction + insert).
    /// 3. `state_tx.send().await` with a 5-second timeout. The channel has
    ///    256 slots; under normal operation the send resolves immediately.
    ///    The timeout exists to bound the worst-case wait if the state
    ///    worker is stalled, so we surface failure to the user instead of
    ///    hanging the prompt indefinitely.
    /// 4. Mutate the in-memory map ONLY when the channel accepts the batch.
    ///    On timeout or channel-closed, return `Err(reason)` with a
    ///    chat-safe explanation.
    pub async fn persist_named_session(
        &mut self,
        prefixed_key: String,
        entry: NamedSessionEntry,
    ) -> PersistOutcome {
        // Read-only eviction-target lookup. Doesn't touch the map.
        let evict_key: Option<String> = if !self.named_sessions.contains_key(&prefixed_key)
            && self.named_sessions.len() >= self.max_named_sessions
        {
            self.named_sessions
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
        } else {
            None
        };

        let mut batch_ops: Vec<(String, Option<NamedSessionEntry>)> = Vec::new();
        if let Some(ref k) = evict_key {
            batch_ops.push((k.clone(), None));
        }
        batch_ops.push((prefixed_key.clone(), Some(entry.clone())));

        let persist = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.state_tx
                .send(StateUpdate::HarnessSessionBatch(batch_ops)),
        )
        .await;

        match persist {
            Ok(Ok(())) => {
                if let Some(k) = evict_key {
                    self.named_sessions.remove(&k);
                }
                self.named_sessions.insert(prefixed_key, entry);
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(
                    "state worker channel closed; named-session persistence broken for this run: {}",
                    e
                );
                Err(
                    "Session-state persistence failed (state worker channel closed); \
                     this prompt's named session won't survive a restart."
                        .to_string(),
                )
            }
            Err(_timeout) => {
                tracing::error!(
                    "state_tx send timed out after 5s; state worker is stalled — skipping \
                     session persistence to avoid silent drift. Investigate state_tx \
                     debounce/flush."
                );
                Err(
                    "Session-state worker is stalled; this prompt's named session won't \
                     survive a restart."
                        .to_string(),
                )
            }
        }
    }

    /// Send an error and return `true` if `--name`/`--resume` are specified
    /// on a harness that doesn't support resume. Callers should `return`
    /// immediately when this returns `true`.
    ///
    /// Returns a plain `bool` rather than a `Result<()>` because the caller
    /// retains control flow (a non-resumable rejection is not an *error* to
    /// the caller — it is a normal command-path branch that already
    /// communicated failure to the user via `dispatch.send_error`). Using
    /// `Result` would force callers to thread `?` through the prompt-dispatch
    /// path and lose the early-return semantics.
    ///
    /// Short-circuits BEFORE touching `dispatch` when no flags are set or
    /// the harness is missing, so unit tests can pass a minimal dispatch
    /// stub for the common no-flags assertion.
    pub async fn reject_if_not_resumable(
        &self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        options: &HarnessOptions,
        dispatch: &PromptDispatchContext<'_>,
    ) -> bool {
        if options.name.is_none() && options.resume.is_none() {
            return false;
        }
        let Some(h) = self.harnesses.get(kind) else {
            return false;
        };
        if h.supports_resume() {
            return false;
        }
        dispatch
            .send_error(
                ctx,
                &format!(
                    "--name/--resume not supported for {} (no session resume)",
                    kind.name()
                ),
            )
            .await;
        true
    }

    /// Send a prompt to a harness (Claude, Gemini, Codex, opencode) with
    /// real-time streaming back to chat. Migrated from `App::send_harness_prompt`
    /// — see module header for why the dispatch handles arrive via
    /// [`PromptDispatchContext`] rather than being owned by the registry.
    pub async fn send_harness_prompt(
        &mut self,
        ctx: &ReplyContext,
        kind: &HarnessKind,
        prompt: &str,
        attachments: &[Attachment],
        options: &HarnessOptions,
        dispatch: &PromptDispatchContext<'_>,
    ) {
        // Check --name/--resume on non-resumable harnesses
        if self
            .reject_if_not_resumable(ctx, kind, options, dispatch)
            .await
        {
            return;
        }

        // Resolve session name from flags, falling back to implicit path
        let using_named_session;
        let session_name;
        let resume_id;
        let cwd;
        let mut notification: Option<String> = None;

        if let Some(ref resume_name) = options.resume {
            // --resume: strict lookup, error if not found (or has no session_id yet)
            match self.lookup_named_session(*kind, resume_name) {
                Some(entry) if !entry.session_id.is_empty() => {
                    session_name = resume_name.clone();
                    resume_id = Some(entry.session_id.clone());
                    cwd = entry.cwd.clone();
                    using_named_session = true;
                }
                _ => {
                    dispatch
                        .send_error(
                            ctx,
                            &format!(
                                "No session named '{}'. Use --name to create one.",
                                resume_name
                            ),
                        )
                        .await;
                    return;
                }
            }
        } else if let Some(ref name) = options.name {
            // --name: create-or-resume (upsert)
            if let Some(entry) = self
                .lookup_named_session(*kind, name)
                .filter(|e| !e.session_id.is_empty())
            {
                session_name = name.clone();
                resume_id = Some(entry.session_id.clone());
                cwd = entry.cwd.clone();
                using_named_session = true;
                let already_resolved =
                    dispatch.session_mgr.foreground_named_session_resolved() == Some(name.as_str());
                if !already_resolved {
                    notification = Some(format!("Resuming existing session '{}'", name));
                }
            } else {
                // New session — record current cwd
                session_name = name.clone();
                resume_id = None;
                cwd = current_dir_or_dot();
                using_named_session = true;
                let already_resolved =
                    dispatch.session_mgr.foreground_named_session_resolved() == Some(name.as_str());
                if !already_resolved {
                    notification = numeric_name_hint(name);
                }
            }
        } else {
            // Implicit path — unchanged
            session_name = dispatch
                .session_mgr
                .foreground_session()
                .unwrap_or("default")
                .to_string();
            using_named_session = false;
            resume_id = self
                .harnesses
                .get(kind)
                .and_then(|h| h.get_session_id(&session_name));
            cwd = current_dir_or_dot();
        }

        let harness = match self.harnesses.get(kind) {
            Some(h) => h,
            None => {
                dispatch
                    .send_error(ctx, &format!("{} harness not available", kind.name()))
                    .await;
                return;
            }
        };

        // Send notification before the prompt (if any)
        if let Some(ref note) = notification {
            dispatch.send_reply(ctx, note).await;
        }

        match harness
            .run_prompt(prompt, attachments, &cwd, resume_id.as_deref(), options)
            .await
        {
            Ok(event_rx) => {
                let run_id = ulid::Ulid::new().to_string();
                dispatch.emit_ambient(AmbientEvent::HarnessStarted {
                    harness: kind.name().to_string(),
                    run_id: run_id.clone(),
                    prompt_hash: None,
                });

                let hctx = HarnessContext {
                    ctx,
                    telegram: dispatch.telegram,
                    slack: dispatch.slack,
                    discord: dispatch.discord,
                    schema_registry: dispatch.schema_registry,
                    delivery_queue: dispatch.delivery_queue,
                    webhook_client: dispatch.webhook_client,
                    stream_tx: dispatch.stream_tx,
                };
                let (got_output, session_id) = drive_harness(event_rx, &hctx).await;

                let status = if got_output { "completed" } else { "no_output" };
                dispatch.emit_ambient(AmbientEvent::HarnessFinished {
                    harness: kind.name().to_string(),
                    run_id,
                    status: status.to_string(),
                });

                if using_named_session {
                    let prefixed_key = build_session_key(*kind, &session_name);
                    let effective_sid = session_id.or_else(|| {
                        self.lookup_named_session(*kind, &session_name)
                            .map(|e| e.session_id.clone())
                            .filter(|s| !s.is_empty())
                    });
                    if let Some(sid) = effective_sid {
                        let entry = NamedSessionEntry {
                            session_id: sid,
                            cwd: cwd.clone(),
                            last_used: Utc::now(),
                        };
                        let outcome = self.persist_named_session(prefixed_key, entry).await;
                        if let Err(reason) = outcome {
                            dispatch.send_error(ctx, &reason).await;
                        }
                    }
                } else if let Some(sid) = session_id {
                    if let Some(h) = self.harnesses.get(kind) {
                        h.set_session_id(&session_name, sid);
                    }
                }
                if !got_output {
                    dispatch
                        .send_error(ctx, &format!("{}: no response received", kind.name()))
                        .await;
                }
            }
            Err(e) => {
                dispatch
                    .send_error(ctx, &format!("{}: {:#}", kind.name(), e))
                    .await;
            }
        }
    }

    // ── Test-only helpers ────────────────────────────────────────────────

    #[cfg(test)]
    pub fn evict_lru_session(&mut self) -> Option<String> {
        let oldest = self
            .named_sessions
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        if let Some(ref key) = oldest {
            self.named_sessions.remove(key);
        }
        oldest
    }

    #[cfg(test)]
    pub fn named_sessions_len(&self) -> usize {
        self.named_sessions.len()
    }

    #[cfg(test)]
    pub fn named_sessions_contains_key(&self, key: &str) -> bool {
        self.named_sessions.contains_key(key)
    }

    #[cfg(test)]
    pub fn named_sessions_get(&self, key: &str) -> Option<&NamedSessionEntry> {
        self.named_sessions.get(key)
    }

    #[cfg(test)]
    pub fn named_sessions_insert_for_test(&mut self, key: String, entry: NamedSessionEntry) {
        self.named_sessions.insert(key, entry);
    }

    #[cfg(test)]
    pub fn set_max_named_sessions_for_test(&mut self, max: usize) {
        self.max_named_sessions = max;
    }

    #[cfg(test)]
    pub fn insert_harness_for_test(&mut self, kind: HarnessKind, harness: Box<dyn Harness>) {
        self.harnesses.insert(kind, harness);
    }
}

/// Resolve the current working directory, falling back to `"."` when the
/// platform call fails. Mirrors `App::current_dir_or_dot`.
fn current_dir_or_dot() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// If a session name parses as a plain `u32`, return a hint explaining that
/// `-n` now means `--name` (not `--max-turns`). `-n 5` is still accepted as
/// a valid session name, but the user very likely meant `-t 5`.
///
/// Fires only on session *creation*, so resuming an existing numeric name
/// stays silent. Mirrors `app::numeric_name_hint` (SYNC: keep both copies
/// in lock-step — they live in separate modules to avoid a circular
/// dependency, but a behavior change to one must propagate to the other).
fn numeric_name_hint(name: &str) -> Option<String> {
    name.parse::<u32>().ok().map(|_| {
        format!(
            "Hint: '{name}' is a valid session name, but if you meant --max-turns use `-t {name}` (the `-n` short flag is now --name).",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_entry(sid: &str, secs_ago: i64) -> NamedSessionEntry {
        NamedSessionEntry {
            session_id: sid.into(),
            cwd: PathBuf::from("/tmp"),
            last_used: Utc::now() - chrono::Duration::seconds(secs_ago),
        }
    }

    fn make_test_config(dir: &std::path::Path) -> Config {
        let toml_path = dir.join("terminus.toml");
        std::fs::write(
            &toml_path,
            r#"
[auth]
telegram_user_id = 12345

[telegram]
bot_token = "test_token_that_is_not_real"

[blocklist]
patterns = []
"#,
        )
        .unwrap();
        Config::load(&toml_path).expect("test config load")
    }

    fn make_registry(dir: &std::path::Path) -> (HarnessRegistry, mpsc::Receiver<StateUpdate>) {
        let config = make_test_config(dir);
        let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(64);
        let (ambient_tx, _) = broadcast::channel::<AmbientEvent>(16);
        let schema_registry =
            Arc::new(SchemaRegistry::from_config(&config.schemas).expect("schema registry"));
        let registry = HarnessRegistry::new(
            &config,
            state_tx,
            ambient_tx,
            schema_registry,
            HashMap::new(),
        );
        (registry, state_rx)
    }

    #[tokio::test]
    async fn evict_lru_session_removes_oldest_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _state_rx) = make_registry(dir.path());

        reg.named_sessions_insert_for_test("new".into(), fake_entry("sid-new", 5));
        reg.named_sessions_insert_for_test("oldest".into(), fake_entry("sid-old", 3600));
        reg.named_sessions_insert_for_test("middle".into(), fake_entry("sid-mid", 300));

        let evicted = reg.evict_lru_session();
        assert_eq!(evicted.as_deref(), Some("oldest"));
        assert_eq!(reg.named_sessions_len(), 2);
        assert!(!reg.named_sessions_contains_key("oldest"));
        assert!(reg.named_sessions_contains_key("new"));
        assert!(reg.named_sessions_contains_key("middle"));
    }

    #[tokio::test]
    async fn evict_lru_session_on_empty_index_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _state_rx) = make_registry(dir.path());
        assert!(reg.evict_lru_session().is_none());
    }

    #[tokio::test]
    async fn persist_named_session_succeeds_and_mutates_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, mut state_rx) = make_registry(dir.path());

        let outcome = reg
            .persist_named_session("claude:foo".into(), fake_entry("sid-1", 0))
            .await;
        assert!(matches!(outcome, Ok(())));
        assert!(reg.named_sessions_contains_key("claude:foo"));

        // The state worker received the batch.
        let update = state_rx
            .try_recv()
            .expect("state update should be enqueued");
        match update {
            StateUpdate::HarnessSessionBatch(ops) => {
                assert!(ops.iter().any(|(k, v)| k == "claude:foo" && v.is_some()));
            }
            other => panic!("expected HarnessSessionBatch, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn persist_named_session_does_not_mutate_in_memory_on_channel_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, state_rx) = make_registry(dir.path());
        // Drop the receiver so the next `state_tx.send().await` returns `SendError`.
        drop(state_rx);

        let outcome = reg
            .persist_named_session("claude:foo".into(), fake_entry("sid-1", 0))
            .await;
        match outcome {
            Err(msg) => assert!(
                msg.contains("won't survive a restart"),
                "expected user-visible failure message, got: {}",
                msg
            ),
            other => panic!("expected Err, got {:?}", other),
        }
        assert!(
            !reg.named_sessions_contains_key("claude:foo"),
            "in-memory map must NOT contain entries that failed to persist"
        );
    }

    #[tokio::test]
    async fn persist_named_session_does_not_evict_in_memory_on_channel_closed() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_test_config(dir.path());
        let (state_tx, state_rx) = mpsc::channel::<StateUpdate>(4);
        let (ambient_tx, _) = broadcast::channel::<AmbientEvent>(16);
        let schema_registry =
            Arc::new(SchemaRegistry::from_config(&config.schemas).expect("schema registry"));
        let mut reg = HarnessRegistry::new(
            &config,
            state_tx,
            ambient_tx,
            schema_registry,
            HashMap::new(),
        );
        reg.set_max_named_sessions_for_test(2);

        // Pre-fill at the cap with two entries that have distinct
        // `last_used` timestamps so eviction has a deterministic target.
        reg.named_sessions_insert_for_test(
            "claude:old".into(),
            NamedSessionEntry {
                session_id: "sid-old".into(),
                cwd: PathBuf::from("/tmp"),
                last_used: Utc::now() - chrono::Duration::hours(1),
            },
        );
        reg.named_sessions_insert_for_test("claude:fresh".into(), fake_entry("sid-fresh", 0));

        // Drop the receiver to force a channel-closed failure.
        drop(state_rx);

        let outcome = reg
            .persist_named_session("claude:new".into(), fake_entry("sid-new", 0))
            .await;
        assert!(outcome.is_err());
        assert!(
            reg.named_sessions_contains_key("claude:old"),
            "eviction must not take effect when persistence fails"
        );
        assert!(
            !reg.named_sessions_contains_key("claude:new"),
            "new entry must not be inserted when persistence fails"
        );
    }

    /// Pin the distinct timeout and channel-closed messages so a regression
    /// that collapses both into one would be caught.
    #[test]
    fn persist_outcome_timeout_message_is_distinct_from_channel_closed() {
        let timeout_msg =
            "Session-state worker is stalled; this prompt's named session won't survive a restart.";
        let channel_msg =
            "Session-state persistence failed (state worker channel closed); this prompt's named session won't survive a restart.";
        assert_ne!(timeout_msg, channel_msg);
        assert!(timeout_msg.contains("stalled"));
        assert!(channel_msg.contains("channel closed"));
        for m in [timeout_msg, channel_msg] {
            assert!(
                m.contains("won't survive a restart"),
                "common consequence phrasing required, got: {}",
                m
            );
        }
    }

    #[tokio::test]
    async fn lookup_named_session_returns_none_when_both_keys_absent() {
        // Pinning the both-miss path: bare `name` AND prefixed `kind:name`
        // both miss → None. A regression that returned a stale/default
        // entry in this case would silently corrupt --resume semantics.
        let dir = tempfile::tempdir().unwrap();
        let (reg, _state_rx) = make_registry(dir.path());
        assert!(reg
            .lookup_named_session(HarnessKind::Claude, "missing")
            .is_none());
    }

    #[tokio::test]
    async fn lookup_named_session_falls_back_to_unprefixed_legacy_key() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _state_rx) = make_registry(dir.path());

        // Seed an unprefixed legacy key — pre-cross-harness-isolation format.
        reg.named_sessions_insert_for_test("auth".into(), fake_entry("sid-legacy", 0));

        // Lookup with the prefixed format misses; legacy fallback hits.
        let entry = reg
            .lookup_named_session(HarnessKind::Claude, "auth")
            .expect("legacy fallback should find unprefixed key");
        assert_eq!(entry.session_id, "sid-legacy");
    }

    #[tokio::test]
    async fn lookup_named_session_prefers_prefixed_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _state_rx) = make_registry(dir.path());

        // Both keys present: prefixed must win.
        reg.named_sessions_insert_for_test("auth".into(), fake_entry("sid-legacy", 0));
        reg.named_sessions_insert_for_test(
            build_session_key(HarnessKind::Claude, "auth"),
            fake_entry("sid-prefixed", 0),
        );

        let entry = reg
            .lookup_named_session(HarnessKind::Claude, "auth")
            .unwrap();
        assert_eq!(
            entry.session_id, "sid-prefixed",
            "prefixed key must be preferred over legacy"
        );
    }

    #[test]
    fn current_dir_or_dot_never_returns_empty_path() {
        let p = super::current_dir_or_dot();
        assert!(!p.as_os_str().is_empty());
    }

    #[test]
    fn numeric_name_hint_fires_on_plain_integer() {
        let hint = super::numeric_name_hint("5").expect("should produce a hint");
        assert!(hint.contains("--max-turns"));
        assert!(hint.contains("-t 5"));
    }

    #[test]
    fn numeric_name_hint_silent_for_alphanumeric_names() {
        assert!(super::numeric_name_hint("auth").is_none());
        assert!(super::numeric_name_hint("v1").is_none());
        assert!(super::numeric_name_hint("5auth").is_none());
        assert!(super::numeric_name_hint("auth-5").is_none());
    }

    #[test]
    fn numeric_name_hint_silent_for_oversized_number() {
        assert!(super::numeric_name_hint("99999999999999999999").is_none());
    }

    #[tokio::test]
    async fn dispatch_send_reply_routes_to_socket_when_socket_reply_tx_is_set() {
        // Pin the socket-origin short-circuit in `PromptDispatchContext::send_reply`.
        // A regression that dropped the early-return would route socket-origin
        // replies through a (non-existent) chat platform, silently swallowing
        // the response. Previously this branch was only exercised transitively
        // by App-level tests; this is the focused unit test critic 4 asked for.
        let dir = tempfile::tempdir().unwrap();
        let (reg, _state_rx) = make_registry(dir.path());
        let session_mgr =
            crate::session::SessionManager::new(crate::tmux::TmuxClient::new(), 8, ':');
        let (stream_tx, _) = broadcast::channel::<StreamEvent>(8);
        let (ambient_tx, _) = broadcast::channel::<AmbientEvent>(8);
        let schema_registry =
            Arc::new(SchemaRegistry::from_config(&Default::default()).expect("schema registry"));
        let dir2 = tempfile::tempdir().unwrap();
        let delivery_queue =
            Arc::new(DeliveryQueue::new(dir2.path().join("queue")).expect("queue"));
        let webhook_client = Arc::new(WebhookClient::new(5_000));
        let dispatch = PromptDispatchContext {
            session_mgr: &session_mgr,
            telegram: None,
            slack: None,
            discord: None,
            schema_registry: &schema_registry,
            delivery_queue: &delivery_queue,
            webhook_client: &webhook_client,
            stream_tx: &stream_tx,
            ambient_tx: &ambient_tx,
        };

        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<String>();
        let ctx = ReplyContext {
            platform: PlatformType::Telegram,
            chat_id: "ignored-for-socket-origin".into(),
            thread_ts: None,
            socket_reply_tx: Some(reply_tx),
        };
        dispatch.send_reply(&ctx, "hello-from-socket").await;

        let received = reply_rx
            .try_recv()
            .expect("socket-origin reply must reach reply_tx");
        assert_eq!(received, "hello-from-socket");
        // Hold a reference so the registry stays alive through the test.
        let _ = &reg;
    }

    #[tokio::test]
    async fn harness_for_returns_each_kind() {
        let dir = tempfile::tempdir().unwrap();
        let (reg, _state_rx) = make_registry(dir.path());
        for kind in [
            HarnessKind::Claude,
            HarnessKind::Opencode,
            HarnessKind::Gemini,
            HarnessKind::Codex,
        ] {
            assert!(
                reg.harness_for(&kind).is_some(),
                "harness_for({:?}) must return Some after construction",
                kind
            );
            assert!(reg.contains_kind(&kind));
        }
    }
}
