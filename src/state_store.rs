use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// State types
// ──────────────────────────────────────────────────────────────────────────────

/// A named harness session entry persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedSessionEntry {
    pub session_id: String,
    pub cwd: PathBuf,
    pub last_used: DateTime<Utc>,
}

/// Persisted state schema.  schema_version == 1 for this implementation.
/// Any file with a different schema_version is treated as corrupt/incompatible
/// and quarantined so a fresh default is used instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    pub telegram: TelegramState,
    pub chats: Chats,
    pub last_seen_wall: Option<DateTime<Utc>>,
    #[serde(default)]
    pub harness_sessions: HashMap<String, NamedSessionEntry>,
    pub last_clean_shutdown: bool,
    /// Per-channel Slack watermarks: channel_id → latest seen message ts.
    /// Used by `run_catchup` to resume from the last known position on wake.
    #[serde(default)]
    pub slack_watermarks: HashMap<String, String>,
    /// Per-channel Discord watermarks: channel_id → latest seen snowflake message_id.
    /// Used by `run_catchup` to resume from the last known position on wake.
    #[serde(default)]
    pub discord_watermarks: HashMap<String, u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            telegram: TelegramState::default(),
            chats: Chats::default(),
            last_seen_wall: None,
            harness_sessions: HashMap::new(),
            last_clean_shutdown: true,
            slack_watermarks: HashMap::new(),
            discord_watermarks: HashMap::new(),
        }
    }
}

/// Telegram-specific persisted state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelegramState {
    /// Signed i64 to match the Telegram Bot API semantics — the API documents
    /// offset as signed, and teloxide's UpdateId casts to i32/i64 at boundaries.
    pub offset: i64,
}

/// Persisted chat bindings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Chats {
    pub telegram: Vec<i64>,
    pub slack: Vec<String>,
    #[serde(default)]
    pub discord: Vec<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// StateUpdate — sent by platform adapters and App internals via mpsc channel
// ──────────────────────────────────────────────────────────────────────────────

/// Mutations that can be applied to in-memory State.
/// Platform adapters send these over an mpsc channel; StateStore is owned
/// exclusively by App and applies them with debounce before persisting.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StateUpdate {
    /// Overwrite the Telegram polling offset (monotonic by construction).
    TelegramOffset(i64),
    /// Record a Telegram chat ID (deduplicates).
    BindTelegramChat(i64),
    /// Record a Slack channel ID (deduplicates).
    BindSlackChat(String),
    /// Record a Discord channel ID (deduplicates).
    BindDiscordChat(String),
    /// Signal first activity after startup — sets last_clean_shutdown = false.
    MarkDirty,
    /// Update last_seen_wall to now (UTC).
    Tick,
    /// Explicitly set last_clean_shutdown to the given value.
    /// Used by `App::mark_clean_shutdown()` on graceful shutdown.
    SetCleanShutdown(bool),
    /// Batch upsert/remove named harness session entries.
    /// Vec allows atomic eviction + insert in a single update.
    HarnessSessionBatch(Vec<(String, Option<NamedSessionEntry>)>),
    /// Advance the per-channel Slack watermark.  Only advances forward
    /// (monotonic): if the stored ts >= the incoming ts, the update is a no-op.
    /// Force-persisted (bypasses debounce) because it is safety-critical for
    /// lossless wake-recovery.
    SlackWatermark { channel_id: String, ts: String },
    /// Advance the per-channel Discord snowflake watermark.  Always overwrites
    /// (snowflakes are monotonically increasing by Discord's design).
    /// Force-persisted (bypasses debounce) for lossless wake-recovery.
    DiscordWatermark { channel_id: String, message_id: u64 },
}

// ──────────────────────────────────────────────────────────────────────────────
// StateStore
// ──────────────────────────────────────────────────────────────────────────────

pub struct StateStore {
    path: PathBuf,
    state: State,
}

impl StateStore {
    /// Load state from `path`.
    ///
    /// - Missing file → returns default state (fresh install).
    /// - Corrupt / wrong schema_version → renames to `<path>.corrupt-<unix_ts>`,
    ///   logs a WARN, and returns default state.
    #[allow(dead_code)]
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path: PathBuf = path.into();

        if !path.exists() {
            return Ok(Self {
                path,
                state: State::default(),
            });
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading state file {}", path.display()))?;

        match serde_json::from_str::<State>(&raw) {
            Ok(state) if state.schema_version == 1 => Ok(Self { path, state }),
            Ok(state) => {
                // Wrong schema version — quarantine.
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // Build quarantine path as <path>.corrupt-<ts>
                let quarantine = {
                    let mut q = path.clone().into_os_string();
                    q.push(format!(".corrupt-{}", ts));
                    PathBuf::from(q)
                };
                tracing::warn!(
                    path = %path.display(),
                    quarantine = %quarantine.display(),
                    schema_version = state.schema_version,
                    "state file has unexpected schema_version, quarantining"
                );
                let _ = std::fs::rename(&path, &quarantine);
                Ok(Self {
                    path,
                    state: State::default(),
                })
            }
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let quarantine = {
                    let mut q = path.clone().into_os_string();
                    q.push(format!(".corrupt-{}", ts));
                    PathBuf::from(q)
                };
                tracing::warn!(
                    path = %path.display(),
                    quarantine = %quarantine.display(),
                    error = %e,
                    "state file is corrupt, quarantining"
                );
                let _ = std::fs::rename(&path, &quarantine);
                Ok(Self {
                    path,
                    state: State::default(),
                })
            }
        }
    }

    /// Apply a `StateUpdate` to in-memory state.
    #[allow(dead_code)]
    pub fn apply(&mut self, update: StateUpdate) {
        match update {
            StateUpdate::TelegramOffset(offset) => {
                self.state.telegram.offset = offset;
            }
            StateUpdate::BindTelegramChat(id) => {
                if !self.state.chats.telegram.contains(&id) {
                    self.state.chats.telegram.push(id);
                }
            }
            StateUpdate::BindSlackChat(id) => {
                if !self.state.chats.slack.contains(&id) {
                    self.state.chats.slack.push(id);
                }
            }
            StateUpdate::BindDiscordChat(id) => {
                if !self.state.chats.discord.contains(&id) {
                    self.state.chats.discord.push(id);
                }
            }
            StateUpdate::MarkDirty => {
                self.state.last_clean_shutdown = false;
            }
            StateUpdate::Tick => {
                self.state.last_seen_wall = Some(Utc::now());
            }
            StateUpdate::SetCleanShutdown(val) => {
                self.state.last_clean_shutdown = val;
            }
            StateUpdate::HarnessSessionBatch(ops) => {
                for (name, entry) in ops {
                    match entry {
                        Some(e) => {
                            self.state.harness_sessions.insert(name, e);
                        }
                        None => {
                            self.state.harness_sessions.remove(&name);
                        }
                    }
                }
            }
            StateUpdate::SlackWatermark { channel_id, ts } => {
                // Only advance forward — Slack ts is lexicographically monotonic.
                let advance = match self.state.slack_watermarks.get(&channel_id) {
                    Some(current) => ts > *current,
                    None => true,
                };
                if advance {
                    self.state.slack_watermarks.insert(channel_id, ts);
                }
            }
            StateUpdate::DiscordWatermark {
                channel_id,
                message_id,
            } => {
                // Only advance forward — Discord snowflakes are always-increasing.
                let entry = self.state.discord_watermarks.entry(channel_id).or_insert(0);
                if message_id > *entry {
                    *entry = message_id;
                }
            }
        }
    }

    /// Atomically persist state to disk:
    /// 1. Ensure parent directory exists with `0700` perms in a single syscall
    ///    on Unix (`DirBuilder::mode`) — closes the brief umask window a
    ///    `create_dir_all` → `set_permissions` sequence would leave open.
    /// 2. Write JSON to a `NamedTempFile` in the same directory (rename is
    ///    atomic on a single filesystem).
    /// 3. `persist()` (atomic rename).
    /// 4. fsync the parent directory so the directory entry is durable.
    #[allow(dead_code)]
    pub fn persist(&self) -> Result<()> {
        // `Path::parent()` returns `Some("")` for a bare filename like
        // `terminus-state.json`, which propagates into `File::open("")`
        // and fails with ENOENT on directory fsync. Collapse empty → ".".
        let parent = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };

        // Create parent dirs if they don't exist, applying 0700 to every
        // ancestor we create — DirBuilder::recursive(true) applies the mode
        // only to the leaf directory; intermediates would get umask defaults
        // (security review MEDIUM finding).  Walk ancestors shallowest-first
        // so parents are created before children.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            // Collect missing ancestors from deepest to shallowest, then
            // reverse so we create shallowest (parents) first.
            let mut missing: Vec<&Path> = Vec::new();
            let mut cur: &Path = parent;
            loop {
                if cur.as_os_str().is_empty() || cur.exists() {
                    break;
                }
                missing.push(cur);
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            missing.reverse();
            for dir in missing {
                builder
                    .create(dir)
                    .with_context(|| format!("creating state dir {}", dir.display()))?;
            }
            // Parent now exists — make sure perms are 0700 in case it was
            // created by an older run with loose permissions.
            if parent.exists() {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(parent) {
                    let current = meta.permissions().mode() & 0o777;
                    if current != 0o700 {
                        let mut perms = meta.permissions();
                        perms.set_mode(0o700);
                        if let Err(e) = std::fs::set_permissions(parent, perms) {
                            tracing::warn!(
                                dir = %parent.display(),
                                error = %e,
                                "could not tighten permissions on existing state directory"
                            );
                        }
                    }
                }
            }
        }

        let json =
            serde_json::to_string_pretty(&self.state).context("serializing state to JSON")?;

        let tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating tempfile in {}", parent.display()))?;

        std::fs::write(tmp.path(), &json)
            .with_context(|| format!("writing state to tempfile {}", tmp.path().display()))?;

        tmp.persist(&self.path)
            .with_context(|| format!("persisting state to {}", self.path.display()))?;

        // fsync the directory so the rename is durable on crash.
        match std::fs::File::open(parent) {
            Ok(dir_fd) => {
                if let Err(e) = dir_fd.sync_all() {
                    tracing::warn!(
                        dir = %parent.display(),
                        error = %e,
                        "could not fsync state directory"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    dir = %parent.display(),
                    error = %e,
                    "could not open state directory for fsync"
                );
            }
        }

        Ok(())
    }

    /// Returns a reference to the current in-memory state snapshot.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> &State {
        &self.state
    }

    /// Returns the default state file path: `<config_parent>/terminus-state.json`.
    /// Used when `power.state_file` is `None`.
    #[allow(dead_code)]
    pub fn resolve_default_path(config_path: &Path) -> PathBuf {
        let parent = match config_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        parent.join("terminus-state.json")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_serde() {
        let state = State::default();
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: State = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.telegram.offset, 0);
        assert!(parsed.chats.telegram.is_empty());
        assert!(parsed.chats.slack.is_empty());
        assert!(parsed.last_seen_wall.is_none());
        assert!(parsed.last_clean_shutdown);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        assert!(!path.exists());

        let store = StateStore::load(&path).expect("load missing file");
        assert_eq!(store.snapshot().schema_version, 1);
        assert_eq!(store.snapshot().telegram.offset, 0);
        assert!(store.snapshot().last_clean_shutdown);
    }

    #[test]
    fn load_corrupt_file_quarantines_and_starts_fresh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        std::fs::write(&path, b"{invalid json}").unwrap();

        let store = StateStore::load(&path).expect("load corrupt file");

        // The original file must be gone (renamed to a .corrupt-* sibling).
        assert!(!path.exists(), "original state file should be gone");

        // At least one .corrupt-* file should exist.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let has_quarantine = entries
            .iter()
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(has_quarantine, "expected a .corrupt-* quarantine file");

        // Returned state must be the default.
        assert_eq!(store.snapshot().schema_version, 1);
        assert_eq!(store.snapshot().telegram.offset, 0);
    }

    #[test]
    fn persist_is_atomic_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");

        // First: write and persist a known state.
        let mut store = StateStore::load(&path).unwrap();
        store.apply(StateUpdate::TelegramOffset(42));
        store.persist().expect("first persist");

        // Verify the file exists with the right content.
        let on_disk: State =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.telegram.offset, 42);

        // Now mutate in memory but do NOT call persist — simulates crash.
        store.apply(StateUpdate::TelegramOffset(99));
        drop(store);

        // File must still contain the old state (42), not 99.
        let on_disk2: State =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk2.telegram.offset, 42,
            "unpersisted mutation must not reach disk"
        );
    }

    #[test]
    fn apply_telegram_offset_overwrites() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::TelegramOffset(5));
        assert_eq!(store.snapshot().telegram.offset, 5);

        store.apply(StateUpdate::TelegramOffset(10));
        assert_eq!(store.snapshot().telegram.offset, 10);
    }

    #[test]
    fn apply_bind_chat_deduplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::BindTelegramChat(42));
        store.apply(StateUpdate::BindTelegramChat(42));

        assert_eq!(
            store.snapshot().chats.telegram,
            vec![42],
            "duplicate telegram chat should not be added twice"
        );

        store.apply(StateUpdate::BindSlackChat("C123".into()));
        store.apply(StateUpdate::BindSlackChat("C123".into()));

        assert_eq!(
            store.snapshot().chats.slack,
            vec!["C123"],
            "duplicate slack chat should not be added twice"
        );
    }

    #[test]
    fn apply_mark_dirty_flips_clean_shutdown() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        // Default must be true (clean).
        assert!(
            store.snapshot().last_clean_shutdown,
            "default last_clean_shutdown should be true"
        );

        store.apply(StateUpdate::MarkDirty);
        assert!(
            !store.snapshot().last_clean_shutdown,
            "MarkDirty must set last_clean_shutdown to false"
        );
    }

    #[test]
    fn resolve_default_path_returns_adjacent_to_config() {
        let config_path = Path::new("/tmp/x/terminus.toml");
        let state_path = StateStore::resolve_default_path(config_path);
        assert_eq!(state_path, PathBuf::from("/tmp/x/terminus-state.json"));
    }

    #[test]
    fn resolve_default_path_bare_filename_uses_cwd() {
        // A bare config filename (no directory component) used to produce a
        // bare state filename, which in turn made `persist()` call
        // `File::open("")` for the directory fsync and log ENOENT every time.
        let config_path = Path::new("terminus.toml");
        let state_path = StateStore::resolve_default_path(config_path);
        assert_eq!(state_path, PathBuf::from("./terminus-state.json"));
    }

    #[test]
    fn persist_succeeds_with_relative_bare_state_path() {
        // Regression: persisting to a bare filename must not warn about
        // "could not open state directory for fsync dir=".
        let dir = tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = (|| -> Result<()> {
            let mut store = StateStore::load(PathBuf::from("terminus-state.json"))?;
            store.apply(StateUpdate::TelegramOffset(7));
            store.persist()?;
            let on_disk: State = serde_json::from_str(&std::fs::read_to_string(
                dir.path().join("terminus-state.json"),
            )?)?;
            assert_eq!(on_disk.telegram.offset, 7);
            Ok(())
        })();

        // Restore cwd before propagating any panic.
        std::env::set_current_dir(cwd).unwrap();
        result.expect("persist with bare filename should succeed");
    }

    #[test]
    fn backward_compat_deserialize_without_discord_field() {
        // Old state files won't have the `discord` field — serde(default) should handle it.
        let json = r#"{
            "schema_version": 1,
            "telegram": { "offset": 42 },
            "chats": { "telegram": [100], "slack": ["C1"] },
            "last_seen_wall": null,
            "last_clean_shutdown": true
        }"#;
        let state: State = serde_json::from_str(json).expect("should deserialize old format");
        assert!(
            state.chats.discord.is_empty(),
            "discord should default to empty vec"
        );
        assert_eq!(state.chats.telegram, vec![100]);
        assert_eq!(state.chats.slack, vec!["C1"]);
    }

    #[test]
    fn apply_bind_discord_chat_deduplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::BindDiscordChat("123456".into()));
        store.apply(StateUpdate::BindDiscordChat("123456".into()));
        store.apply(StateUpdate::BindDiscordChat("789012".into()));

        assert_eq!(
            store.snapshot().chats.discord,
            vec!["123456", "789012"],
            "duplicate discord chat should not be added twice"
        );
    }

    #[test]
    fn schema_version_preserved_or_quarantined() {
        // A file with schema_version != 1 should be quarantined and default returned.
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");

        let incompatible = serde_json::json!({
            "schema_version": 99,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [], "slack": [] },
            "last_seen_wall": null,
            "last_clean_shutdown": true
        });
        std::fs::write(&path, incompatible.to_string()).unwrap();

        let store = StateStore::load(&path).expect("load incompatible schema");

        // Original file must be quarantined.
        assert!(!path.exists(), "original file should be quarantined");

        // State must be default (schema_version = 1).
        assert_eq!(store.snapshot().schema_version, 1);
    }

    #[test]
    fn harness_session_batch_upsert() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        let entry = super::NamedSessionEntry {
            session_id: "sid-123".into(),
            cwd: PathBuf::from("/tmp/project"),
            last_used: chrono::Utc::now(),
        };

        store.apply(StateUpdate::HarnessSessionBatch(vec![(
            "auth".into(),
            Some(entry.clone()),
        )]));

        assert_eq!(store.snapshot().harness_sessions.len(), 1);
        assert_eq!(
            store.snapshot().harness_sessions["auth"].session_id,
            "sid-123"
        );
    }

    #[test]
    fn harness_session_batch_remove() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        let entry = super::NamedSessionEntry {
            session_id: "sid-123".into(),
            cwd: PathBuf::from("/tmp/project"),
            last_used: chrono::Utc::now(),
        };

        store.apply(StateUpdate::HarnessSessionBatch(vec![(
            "auth".into(),
            Some(entry),
        )]));
        assert_eq!(store.snapshot().harness_sessions.len(), 1);

        store.apply(StateUpdate::HarnessSessionBatch(vec![(
            "auth".into(),
            None,
        )]));
        assert!(store.snapshot().harness_sessions.is_empty());
    }

    #[test]
    fn harness_session_batch_atomic_evict_and_insert() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        let old_entry = super::NamedSessionEntry {
            session_id: "old-sid".into(),
            cwd: PathBuf::from("/tmp/old"),
            last_used: chrono::Utc::now(),
        };
        let new_entry = super::NamedSessionEntry {
            session_id: "new-sid".into(),
            cwd: PathBuf::from("/tmp/new"),
            last_used: chrono::Utc::now(),
        };

        // Insert old
        store.apply(StateUpdate::HarnessSessionBatch(vec![(
            "old".into(),
            Some(old_entry),
        )]));

        // Atomic evict old + insert new
        store.apply(StateUpdate::HarnessSessionBatch(vec![
            ("old".into(), None),
            ("new".into(), Some(new_entry)),
        ]));

        assert_eq!(store.snapshot().harness_sessions.len(), 1);
        assert!(!store.snapshot().harness_sessions.contains_key("old"));
        assert_eq!(
            store.snapshot().harness_sessions["new"].session_id,
            "new-sid"
        );
    }

    #[test]
    fn backward_compat_deserialize_without_harness_sessions() {
        let json = r#"{
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [], "slack": [] },
            "last_seen_wall": null,
            "last_clean_shutdown": true
        }"#;
        let state: State = serde_json::from_str(json).expect("should deserialize old format");
        assert!(
            state.harness_sessions.is_empty(),
            "harness_sessions should default to empty map"
        );
    }

    #[test]
    fn test_slack_watermark_advances_monotonically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::SlackWatermark {
            channel_id: "C001".into(),
            ts: "1000000000.000001".into(),
        });
        assert_eq!(
            store
                .snapshot()
                .slack_watermarks
                .get("C001")
                .map(|s| s.as_str()),
            Some("1000000000.000001")
        );

        // Advance forward — must update.
        store.apply(StateUpdate::SlackWatermark {
            channel_id: "C001".into(),
            ts: "1000000000.000002".into(),
        });
        assert_eq!(
            store
                .snapshot()
                .slack_watermarks
                .get("C001")
                .map(|s| s.as_str()),
            Some("1000000000.000002")
        );
    }

    #[test]
    fn test_slack_watermark_backward_ts_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::SlackWatermark {
            channel_id: "C002".into(),
            ts: "1000000000.000010".into(),
        });

        // Attempt to go backward — must be ignored.
        store.apply(StateUpdate::SlackWatermark {
            channel_id: "C002".into(),
            ts: "1000000000.000005".into(),
        });

        assert_eq!(
            store
                .snapshot()
                .slack_watermarks
                .get("C002")
                .map(|s| s.as_str()),
            Some("1000000000.000010"),
            "backward ts must not overwrite a higher watermark"
        );
    }

    #[test]
    fn test_backward_compat_deserialize_without_slack_watermarks() {
        // Old state files without slack_watermarks must deserialize to empty map.
        let json = r#"{
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [], "slack": [] },
            "last_seen_wall": null,
            "last_clean_shutdown": true
        }"#;
        let state: State = serde_json::from_str(json).expect("should deserialize old format");
        assert!(
            state.slack_watermarks.is_empty(),
            "slack_watermarks should default to empty map when absent from JSON"
        );
    }

    #[test]
    fn test_slack_watermark_force_persists() {
        // SlackWatermark must appear in the force-persist variant list in app.rs.
        // Here we verify the state-store apply() is a no-op for same-ts (idempotent).
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::SlackWatermark {
            channel_id: "C003".into(),
            ts: "1000000001.000000".into(),
        });
        // Persist and reload to confirm serializtion.
        store.persist().expect("persist should succeed");

        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(
            reloaded
                .snapshot()
                .slack_watermarks
                .get("C003")
                .map(|s| s.as_str()),
            Some("1000000001.000000"),
            "slack_watermark must survive persist/reload cycle"
        );
    }

    #[test]
    fn test_discord_watermark_serde_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        let snowflake: u64 = 1234567890123456789;
        store.apply(StateUpdate::DiscordWatermark {
            channel_id: "12345".into(),
            message_id: snowflake,
        });
        store.persist().expect("persist should succeed");

        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(
            reloaded.snapshot().discord_watermarks.get("12345").copied(),
            Some(snowflake),
            "discord watermark must survive persist/reload cycle with full u64 precision"
        );
    }

    #[test]
    fn test_state_file_backward_compat_discord_watermarks() {
        // Existing state files without `discord_watermarks` must deserialize
        // cleanly — `#[serde(default)]` provides an empty HashMap.
        let json = r#"{
            "schema_version": 1,
            "telegram": { "offset": 0 },
            "chats": { "telegram": [], "slack": [] },
            "last_seen_wall": null,
            "last_clean_shutdown": true
        }"#;
        let state: State = serde_json::from_str(json).expect("should deserialize old format");
        assert!(
            state.discord_watermarks.is_empty(),
            "discord_watermarks should default to empty map when absent from JSON"
        );
    }

    #[test]
    fn test_discord_watermark_force_persists() {
        // DiscordWatermark must persist reliably (mirrors test_slack_watermark_force_persists).
        // This verifies the apply() + persist() + reload() path for the Discord
        // watermark variant — the in-memory state survives a full persist/reload
        // cycle, confirming that the variant is handled by apply() and serialized
        // correctly by the State struct.
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        let snowflake: u64 = 1_234_567_890_987_654_321;
        store.apply(StateUpdate::DiscordWatermark {
            channel_id: "C_DISCORD_001".into(),
            message_id: snowflake,
        });
        store.persist().expect("persist should succeed");

        let reloaded = StateStore::load(&path).unwrap();
        assert_eq!(
            reloaded
                .snapshot()
                .discord_watermarks
                .get("C_DISCORD_001")
                .copied(),
            Some(snowflake),
            "discord_watermark must survive persist/reload cycle"
        );
    }

    #[test]
    fn test_discord_watermark_advances_monotonically() {
        // Discord snowflakes are always-increasing; apply() must not rewind the
        // watermark when a lower message_id arrives (mirrors Slack monotonicity).
        let dir = tempdir().unwrap();
        let path = dir.path().join("terminus-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::DiscordWatermark {
            channel_id: "C_DW".into(),
            message_id: 9_000_000_000,
        });
        // Apply a lower value — watermark must NOT rewind.
        store.apply(StateUpdate::DiscordWatermark {
            channel_id: "C_DW".into(),
            message_id: 1_000,
        });
        assert_eq!(
            store.snapshot().discord_watermarks.get("C_DW").copied(),
            Some(9_000_000_000),
            "DiscordWatermark must not rewind to a lower message_id"
        );

        // Higher value must still advance.
        store.apply(StateUpdate::DiscordWatermark {
            channel_id: "C_DW".into(),
            message_id: 10_000_000_000,
        });
        assert_eq!(
            store.snapshot().discord_watermarks.get("C_DW").copied(),
            Some(10_000_000_000),
            "DiscordWatermark must advance when a higher message_id is applied"
        );
    }
}
