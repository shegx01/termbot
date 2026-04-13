use std::collections::HashSet;

use crate::tmux::TmuxClient;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    NewMessage {
        session: String,
        content: String,
    },
    SessionExited {
        session: String,
        code: Option<i32>,
    },
    SessionStarted {
        session: String,
        chat_id: String,
        thread_ts: Option<String>,
    },
    /// Emitted once per active chat when a sleep/wake gap is detected or an
    /// unclean restart is detected.  The delivery task renders a human-readable
    /// banner and sends `DeliveryAck::BannerSent` when done.
    GapBanner {
        chat_id: String,
        paused_at: chrono::DateTime<chrono::Utc>,
        resumed_at: chrono::DateTime<chrono::Utc>,
        gap: std::time::Duration,
        missed_count: u32,
    },
}

/// Captures terminal output by diffing tmux pane content between polls.
///
/// Uses content-based change detection: stores the raw capture text before
/// each command, then on each poll compares the current capture to find
/// new lines. This works regardless of scrollback buffer state, terminal
/// size, or full-screen applications (alternate screen).
///
/// For Telegram: each command's output is sent as a NEW message (not edited in place).
pub struct OutputBuffer {
    session_name: String,
    /// Raw pane capture taken before the command was sent.
    /// New output = lines in current capture that weren't in this snapshot.
    snapshot: String,
    last_command: Option<String>,
    offline_buffer: String,
    offline_buffer_max_bytes: usize,
    connected: bool,
    waiting_for_output: bool,
}

impl OutputBuffer {
    pub fn new(session_name: &str, offline_buffer_max_bytes: usize) -> Self {
        Self {
            session_name: session_name.to_string(),
            snapshot: String::new(),
            last_command: None,
            offline_buffer: String::new(),
            offline_buffer_max_bytes,
            connected: true,
            waiting_for_output: false,
        }
    }

    #[allow(dead_code)]
    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    #[allow(dead_code)]
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Snapshot the current pane content before sending a command.
    /// After this, `poll()` will detect changes by comparing against this snapshot.
    pub async fn snapshot_before_command(&mut self, tmux: &TmuxClient, command: Option<&str>) {
        if let Ok(capture) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            self.snapshot = capture;
        }
        self.last_command = command.map(|s| s.to_string());
        self.waiting_for_output = true;
    }

    /// Initialize the snapshot to the current pane content (call on session creation/reconnect).
    pub async fn sync_offset(&mut self, tmux: &TmuxClient) {
        if let Ok(capture) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            self.snapshot = capture;
        }
    }

    /// Poll for new output by comparing current pane content against the snapshot.
    /// Returns new, non-noise lines as StreamEvents.
    pub async fn poll(&mut self, tmux: &TmuxClient) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if !self.waiting_for_output {
            return events;
        }

        let current = match tmux.capture_pane_with_scrollback(&self.session_name).await {
            Ok(s) => s,
            Err(_) => return events,
        };

        // Fast path: nothing changed
        if current == self.snapshot {
            return events;
        }

        tracing::info!(
            "[poll {}] content changed ({} -> {} bytes)",
            self.session_name,
            self.snapshot.len(),
            current.len()
        );

        // Content changed — find lines that are new (not in the snapshot).
        // Use a set of snapshot lines for O(1) lookup.
        let old_lines: HashSet<&str> = self.snapshot.lines().map(|l| l.trim()).collect();

        let cmd = self.last_command.take();
        let content: String = current
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| !old_lines.contains(l.trim()))
            .filter(|l| !is_noise_line(l, cmd.as_deref()))
            .collect::<Vec<&str>>()
            .join("\n")
            .trim()
            .to_string();

        // Update snapshot to current state so we don't re-emit the same output
        self.snapshot = current;
        if content.is_empty() {
            return events;
        }

        if !self.connected {
            self.offline_buffer.push_str(&content);
            self.offline_buffer.push('\n');
            self.truncate_offline_buffer();
            return events;
        }

        events.push(StreamEvent::NewMessage {
            session: self.session_name.clone(),
            content,
        });

        events
    }

    #[allow(dead_code)]
    pub fn reconnect(&mut self) -> Option<StreamEvent> {
        self.connected = true;
        if self.offline_buffer.is_empty() {
            return None;
        }
        let content = std::mem::take(&mut self.offline_buffer);
        Some(StreamEvent::NewMessage {
            session: self.session_name.clone(),
            content: format!("[Catch-up]\n{}", content),
        })
    }

    fn truncate_offline_buffer(&mut self) {
        if self.offline_buffer.len() > self.offline_buffer_max_bytes {
            let excess = self.offline_buffer.len() - self.offline_buffer_max_bytes;
            let safe_start = self.offline_buffer[excess..]
                .char_indices()
                .next()
                .map(|(i, _)| excess + i)
                .unwrap_or(self.offline_buffer.len());
            let truncated_msg = format!("[... {} bytes truncated ...]\n", safe_start);
            self.offline_buffer =
                format!("{}{}", truncated_msg, &self.offline_buffer[safe_start..]);
        }
    }
}

/// Check if a single line is terminal noise (prompt, command echo, eval errors).
fn is_noise_line(line: &str, command: Option<&str>) -> bool {
    let t = line.trim();

    if t.is_empty() {
        return false;
    }

    // Echoed command line
    if let Some(cmd) = command {
        let cmd_t = cmd.trim();
        if t == cmd_t {
            return true;
        }
        for prefix in &["$ ", "% ", "# "] {
            if let Some(after) = t.strip_prefix(prefix) {
                if after.trim() == cmd_t {
                    return true;
                }
            }
        }
        if t.ends_with(cmd_t) && t.len() < cmd_t.len() + 10 {
            return true;
        }
    }

    // Bare prompt characters
    if t == "$" || t == "%" || t == "#" {
        return true;
    }

    // Shell plugin errors
    if t.starts_with("(eval):") && t.contains("command not found") {
        return true;
    }

    // Starship prompt: contains git branch icon () followed by branch name
    if t.contains(" on ") && (t.contains("\u{e0a0}") || t.contains("")) {
        return true;
    }
    if t.contains("📦 v") || t.contains("🦀 v") || t.contains("☁️") {
        return true;
    }
    if t.contains("FORGE") && t.contains("anthropic.") {
        return true;
    }
    if t.starts_with('󱙺') {
        return true;
    }
    // Prompt-only lines (no spaces = likely bare prompt path)
    if (t.ends_with('$') || t.ends_with('%') || t.ends_with('#')) && !t.contains(' ') {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_detection_eval_error() {
        assert!(is_noise_line(
            "(eval):4605: command not found: compdef",
            None
        ));
    }

    #[test]
    fn noise_detection_starship_prompt() {
        assert!(is_noise_line(
            "termbot on  main ()   [?] is 📦 v0.1.0 via 🦀 v1.76.0",
            None
        ));
    }

    #[test]
    fn noise_detection_command_echo() {
        assert!(is_noise_line("$ echo hello", Some("echo hello")));
        assert!(is_noise_line("% echo hello", Some("echo hello")));
        assert!(is_noise_line("echo hello", Some("echo hello")));
    }

    #[test]
    fn real_output_preserved() {
        assert!(!is_noise_line("hello", None));
        assert!(!is_noise_line("total 42", None));
        assert!(!is_noise_line("Hello! How can I help you today?", None));
        assert!(!is_noise_line(
            "drwxr-xr-x  5 user staff  160 Apr 11 src",
            None
        ));
    }

    #[test]
    fn noise_detection_forge_line() {
        assert!(is_noise_line(
            "# claude -p \"hello\"",
            Some("claude -p \"hello\"")
        ));
    }

    #[test]
    fn noise_bare_prompt_chars() {
        assert!(is_noise_line("$", None));
        assert!(is_noise_line("%", None));
        assert!(is_noise_line("#", None));
    }

    #[test]
    fn noise_emoji_prompt() {
        assert!(is_noise_line("something 📦 v0.1.0 via 🦀 v1.76.0", None));
    }

    #[test]
    fn git_output_not_noise() {
        assert!(!is_noise_line("On branch main", None));
        assert!(!is_noise_line("Switched to branch 'main'", None));
        assert!(!is_noise_line("Merge branch 'main' into feature", None));
    }

    #[test]
    fn empty_line_not_noise() {
        assert!(!is_noise_line("", None));
        assert!(!is_noise_line("  ", None));
    }
}
