use anyhow::{bail, Result};
use std::collections::HashMap;
use tokio::time::Instant;

use crate::command::HarnessOptions;
use crate::harness::HarnessKind;
use crate::tmux::TmuxClient;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionStatus {
    Foreground,
    Background,
}

#[derive(Debug)]
pub struct SessionState {
    pub name: String,
    pub status: SessionStatus,
    pub created_at: Instant,
    pub active_harness: Option<HarnessKind>,
    pub harness_options: HarnessOptions,
}

pub struct SessionManager {
    tmux: TmuxClient,
    sessions: HashMap<String, SessionState>,
    foreground: Option<String>,
    max_sessions: usize,
    trigger: char,
}

impl SessionManager {
    pub fn new(tmux: TmuxClient, max_sessions: usize, trigger: char) -> Self {
        Self {
            tmux,
            sessions: HashMap::new(),
            foreground: None,
            max_sessions,
            trigger,
        }
    }

    pub fn tmux(&self) -> &TmuxClient {
        &self.tmux
    }

    /// Number of currently tracked sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Configured maximum session limit.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// Reconnect to an existing tmux session (e.g. after restart).
    pub async fn reconnect_session(&mut self, name: &str) -> Result<()> {
        if self.sessions.contains_key(name) {
            bail!("Session '{}' already tracked", name);
        }
        if !self.tmux.has_session(name).await? {
            bail!("tmux session 'term-{}' does not exist", name);
        }

        let is_first = self.sessions.is_empty();
        self.sessions.insert(
            name.to_string(),
            SessionState {
                name: name.to_string(),
                status: if is_first {
                    SessionStatus::Foreground
                } else {
                    SessionStatus::Background
                },
                created_at: Instant::now(),
                active_harness: None,
                harness_options: HarnessOptions::default(),
            },
        );
        if is_first {
            self.foreground = Some(name.to_string());
        }

        tracing::info!("Reconnected to existing session '{}'", name);
        Ok(())
    }

    pub async fn new_session(&mut self, name: &str) -> Result<()> {
        if self.sessions.contains_key(name) {
            bail!("Session '{}' already exists", name);
        }

        if self.sessions.len() >= self.max_sessions {
            bail!(
                "Maximum session limit reached ({}). Kill an existing session first.",
                self.max_sessions
            );
        }

        if self.tmux.has_session(name).await? {
            bail!("tmux session 'term-{}' already exists externally. Use `{} fg {}` after restart to reconnect.", name, self.trigger, name);
        }

        self.tmux.create_session(name).await?;

        // Background the current foreground session if any
        if let Some(ref fg_name) = self.foreground {
            if let Some(session) = self.sessions.get_mut(fg_name) {
                session.status = SessionStatus::Background;
            }
        }

        self.sessions.insert(
            name.to_string(),
            SessionState {
                name: name.to_string(),
                status: SessionStatus::Foreground,
                created_at: Instant::now(),
                active_harness: None,
                harness_options: HarnessOptions::default(),
            },
        );
        self.foreground = Some(name.to_string());

        Ok(())
    }

    pub fn fg(&mut self, name: &str) -> Result<()> {
        if !self.sessions.contains_key(name) {
            bail!("Session '{}' not found", name);
        }

        if let Some(ref fg_name) = self.foreground {
            if let Some(session) = self.sessions.get_mut(fg_name) {
                session.status = SessionStatus::Background;
            }
        }

        if let Some(session) = self.sessions.get_mut(name) {
            session.status = SessionStatus::Foreground;
        }
        self.foreground = Some(name.to_string());

        Ok(())
    }

    pub fn bg(&mut self) -> Result<Option<String>> {
        match self.foreground.take() {
            Some(fg_name) => {
                if let Some(session) = self.sessions.get_mut(&fg_name) {
                    session.status = SessionStatus::Background;
                }
                Ok(Some(fg_name))
            }
            None => Ok(None),
        }
    }

    pub async fn kill(&mut self, name: &str) -> Result<()> {
        if !self.sessions.contains_key(name) {
            bail!("Session '{}' not found", name);
        }

        self.tmux.kill_session(name).await?;
        self.sessions.remove(name);

        if self.foreground.as_deref() == Some(name) {
            self.foreground = None;
        }

        Ok(())
    }

    pub fn list(&self) -> Vec<(&str, SessionStatus, Instant)> {
        self.sessions
            .values()
            .map(|s| (s.name.as_str(), s.status, s.created_at))
            .collect()
    }

    pub fn foreground_session(&self) -> Option<&str> {
        self.foreground.as_deref()
    }

    /// Get the active harness for the foreground session.
    pub fn foreground_harness(&self) -> Option<HarnessKind> {
        self.foreground
            .as_ref()
            .and_then(|name| self.sessions.get(name))
            .and_then(|s| s.active_harness)
    }

    /// Set the active harness and options for a named session.
    /// When `harness` is `None` (harness off), options are cleared.
    pub fn set_harness(
        &mut self,
        session_name: &str,
        harness: Option<HarnessKind>,
        options: HarnessOptions,
    ) {
        if let Some(session) = self.sessions.get_mut(session_name) {
            session.active_harness = harness;
            session.harness_options = if harness.is_some() {
                options
            } else {
                HarnessOptions::default()
            };
        }
    }

    /// Get the harness options for the foreground session.
    pub fn foreground_harness_options(&self) -> HarnessOptions {
        self.foreground
            .as_ref()
            .and_then(|name| self.sessions.get(name))
            .map(|s| s.harness_options.clone())
            .unwrap_or_default()
    }

    pub async fn health_check(&mut self) -> Vec<(String, Option<i32>)> {
        let mut crashed = Vec::new();
        let names: Vec<String> = self.sessions.keys().cloned().collect();

        for name in names {
            match self.tmux.has_session(&name).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("Session '{}' has exited", name);
                    self.sessions.remove(&name);
                    if self.foreground.as_deref() == Some(name.as_str()) {
                        self.foreground = None;
                    }
                    crashed.push((name, None));
                }
                Err(e) => {
                    tracing::error!("Failed to check session '{}': {}", name, e);
                }
            }
        }

        crashed
    }

    pub async fn execute_in_foreground(&self, cmd: &str) -> Result<()> {
        match &self.foreground {
            Some(name) => self.tmux.send_keys(name, cmd).await,
            None => bail!(
                "No active session. Use `{} new <name>` to create one.",
                self.trigger
            ),
        }
    }

    pub async fn send_stdin_to_foreground(&self, text: &str) -> Result<()> {
        match &self.foreground {
            Some(name) => self.tmux.send_stdin(name, text).await,
            None => bail!(
                "No active session. Use `{} new <name>` to create one.",
                self.trigger
            ),
        }
    }

    /// Detach from all sessions without killing them.
    /// Sessions survive restart and can be reconnected via `reconcile_startup`.
    pub async fn cleanup_all(&mut self) {
        tracing::info!(
            "Detaching from {} session(s) (sessions survive for reconnect)",
            self.sessions.len()
        );
        self.sessions.clear();
        self.foreground = None;
    }
}
