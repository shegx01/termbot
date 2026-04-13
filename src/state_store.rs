use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// State types
// ──────────────────────────────────────────────────────────────────────────────

/// Persisted state schema.  schema_version == 1 for this implementation.
/// Any file with a different schema_version is treated as corrupt/incompatible
/// and quarantined so a fresh default is used instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub schema_version: u32,
    pub telegram: TelegramState,
    pub chats: Chats,
    pub last_seen_wall: Option<DateTime<Utc>>,
    pub last_clean_shutdown: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            telegram: TelegramState::default(),
            chats: Chats::default(),
            last_seen_wall: None,
            last_clean_shutdown: true,
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
    /// Signal first activity after startup — sets last_clean_shutdown = false.
    MarkDirty,
    /// Update last_seen_wall to now (UTC).
    Tick,
    /// Explicitly set last_clean_shutdown to the given value.
    /// Used by `App::mark_clean_shutdown()` on graceful shutdown.
    SetCleanShutdown(bool),
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
            StateUpdate::MarkDirty => {
                self.state.last_clean_shutdown = false;
            }
            StateUpdate::Tick => {
                self.state.last_seen_wall = Some(Utc::now());
            }
            StateUpdate::SetCleanShutdown(val) => {
                self.state.last_clean_shutdown = val;
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
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));

        // Create parent dirs if they don't exist, with 0700 perms on Unix
        // applied at creation time (no umask window).
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            if !parent.exists() {
                builder
                    .create(parent)
                    .with_context(|| format!("creating state dir {}", parent.display()))?;
            } else {
                // Parent already exists — make sure perms are 0700 so we
                // don't leak the file to group/other on re-use.
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
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state dir {}", parent.display()))?;
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

    /// Returns the default state file path: `<config_parent>/termbot-state.json`.
    /// Used when `power.state_file` is `None`.
    #[allow(dead_code)]
    pub fn resolve_default_path(config_path: &Path) -> PathBuf {
        let parent = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        parent.join("termbot-state.json")
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
        let path = dir.path().join("termbot-state.json");
        assert!(!path.exists());

        let store = StateStore::load(&path).expect("load missing file");
        assert_eq!(store.snapshot().schema_version, 1);
        assert_eq!(store.snapshot().telegram.offset, 0);
        assert!(store.snapshot().last_clean_shutdown);
    }

    #[test]
    fn load_corrupt_file_quarantines_and_starts_fresh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("termbot-state.json");
        std::fs::write(&path, b"{invalid json}").unwrap();

        let store = StateStore::load(&path).expect("load corrupt file");

        // The original file must be gone (renamed to a .corrupt-* sibling).
        assert!(!path.exists(), "original state file should be gone");

        // At least one .corrupt-* file should exist.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let has_quarantine = entries.iter().any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains(".corrupt-")
        });
        assert!(has_quarantine, "expected a .corrupt-* quarantine file");

        // Returned state must be the default.
        assert_eq!(store.snapshot().schema_version, 1);
        assert_eq!(store.snapshot().telegram.offset, 0);
    }

    #[test]
    fn persist_is_atomic_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("termbot-state.json");

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
        let path = dir.path().join("termbot-state.json");
        let mut store = StateStore::load(&path).unwrap();

        store.apply(StateUpdate::TelegramOffset(5));
        assert_eq!(store.snapshot().telegram.offset, 5);

        store.apply(StateUpdate::TelegramOffset(10));
        assert_eq!(store.snapshot().telegram.offset, 10);
    }

    #[test]
    fn apply_bind_chat_deduplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("termbot-state.json");
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
        let path = dir.path().join("termbot-state.json");
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
        let config_path = Path::new("/tmp/x/termbot.toml");
        let state_path = StateStore::resolve_default_path(config_path);
        assert_eq!(state_path, PathBuf::from("/tmp/x/termbot-state.json"));
    }

    #[test]
    fn schema_version_preserved_or_quarantined() {
        // A file with schema_version != 1 should be quarantined and default returned.
        let dir = tempdir().unwrap();
        let path = dir.path().join("termbot-state.json");

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
}
