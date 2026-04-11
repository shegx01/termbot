use anyhow::{bail, Result};
use std::collections::HashMap;
use tokio::time::Instant;

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
}

pub struct SessionManager {
    tmux: TmuxClient,
    sessions: HashMap<String, SessionState>,
    foreground: Option<String>,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new(tmux: TmuxClient, max_sessions: usize) -> Self {
        Self {
            tmux,
            sessions: HashMap::new(),
            foreground: None,
            max_sessions,
        }
    }

    pub fn tmux(&self) -> &TmuxClient {
        &self.tmux
    }

    /// Reconnect to an existing tmux session (e.g. after restart).
    pub async fn reconnect_session(&mut self, name: &str) -> Result<()> {
        if self.sessions.contains_key(name) {
            bail!("Session '{}' already tracked", name);
        }
        if !self.tmux.has_session(name).await? {
            bail!("tmux session 'tb-{}' does not exist", name);
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
            bail!("tmux session 'tb-{}' already exists externally. Use `: fg {}` after restart to reconnect.", name, name);
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
            None => bail!("No active session. Use `: new <name>` to create one."),
        }
    }

    pub async fn send_stdin_to_foreground(&self, text: &str) -> Result<()> {
        match &self.foreground {
            Some(name) => self.tmux.send_stdin(name, text).await,
            None => bail!("No active session. Use `: new <name>` to create one."),
        }
    }

    /// Detach from all sessions without killing them.
    /// Sessions survive restart and can be reconnected via `reconcile_startup`.
    pub async fn cleanup_all(&mut self) {
        tracing::info!("Detaching from {} session(s) (sessions survive for reconnect)", self.sessions.len());
        self.sessions.clear();
        self.foreground = None;
    }
}
