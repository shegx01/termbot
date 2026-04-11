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
}

/// Captures terminal output using `tmux capture-pane` with scrollback line tracking.
///
/// Instead of diffing screen snapshots (fragile when content scrolls), this tracks
/// the total number of scrollback lines seen. Each poll captures the full scrollback
/// and only emits lines past the last-seen offset. This is analogous to file-offset
/// tracking but using tmux's scrollback buffer as the source of truth.
///
/// For Telegram: each command's output is sent as a NEW message (not edited in place).
/// This prevents the "wall of text" problem where edit-in-place accumulates the
/// entire session history into one message.
pub struct OutputBuffer {
    session_name: String,
    lines_seen: usize,           // scrollback lines already delivered
    last_command: Option<String>, // command text to strip from echo
    offline_buffer: String,
    offline_buffer_max_bytes: usize,
    connected: bool,
    waiting_for_output: bool,    // true after snapshot_before_command
}

impl OutputBuffer {
    pub fn new(session_name: &str, offline_buffer_max_bytes: usize) -> Self {
        Self {
            session_name: session_name.to_string(),
            lines_seen: 0,
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

    /// Snapshot the current scrollback length before sending a command.
    /// After this, `poll()` will only return lines added after this point.
    pub async fn snapshot_before_command(&mut self, tmux: &TmuxClient, command: Option<&str>) {
        if let Ok(scrollback) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            // Count non-empty trailing lines to get the effective line count
            self.lines_seen = count_meaningful_lines(&scrollback);
        }
        self.last_command = command.map(|s| s.to_string());
        self.waiting_for_output = true;
    }

    /// Initialize the line offset to the current scrollback (call on session creation/reconnect).
    pub async fn sync_offset(&mut self, tmux: &TmuxClient) {
        if let Ok(scrollback) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            self.lines_seen = count_meaningful_lines(&scrollback);
        }
    }

    /// Poll for new output. Returns at most one StreamEvent per poll.
    pub async fn poll(&mut self, tmux: &TmuxClient) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if !self.waiting_for_output {
            return events;
        }

        let scrollback = match tmux.capture_pane_with_scrollback(&self.session_name).await {
            Ok(s) => s,
            Err(_) => return events,
        };

        let all_lines: Vec<&str> = scrollback.lines().collect();
        let meaningful_count = count_meaningful_lines(&scrollback);

        if meaningful_count <= self.lines_seen {
            return events; // no new lines yet — keep polling
        }

        // Extract only the new lines (past our offset)
        // Take from the end of the meaningful lines
        let new_start = self.lines_seen;
        let meaningful_lines: Vec<&str> = all_lines
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();

        let new_lines = if new_start < meaningful_lines.len() {
            &meaningful_lines[new_start..]
        } else {
            return events;
        };

        // Filter out noise
        let cmd = self.last_command.take();
        let filtered: Vec<&str> = new_lines
            .iter()
            .copied()
            .filter(|line| !is_noise_line(line, cmd.as_deref()))
            .collect();

        if filtered.is_empty() {
            // Lines were all noise — update offset but don't emit
            self.lines_seen = meaningful_count;
            return events;
        }

        let content = filtered.join("\n").trim().to_string();
        if content.is_empty() {
            self.lines_seen = meaningful_count;
            return events;
        }

        self.lines_seen = meaningful_count;

        if !self.connected {
            self.offline_buffer.push_str(&content);
            self.offline_buffer.push('\n');
            self.truncate_offline_buffer();
            return events;
        }

        // Send as a new message — clean per-poll output chunk for Telegram.
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
            // Find a safe char boundary at or after `excess`
            let safe_start = self.offline_buffer[excess..]
                .char_indices()
                .next()
                .map(|(i, _)| excess + i)
                .unwrap_or(self.offline_buffer.len());
            let truncated_msg = format!("[... {} bytes truncated ...]\n", safe_start);
            self.offline_buffer = format!("{}{}", truncated_msg, &self.offline_buffer[safe_start..]);
        }
    }
}

/// Count non-empty lines in a scrollback capture.
fn count_meaningful_lines(scrollback: &str) -> usize {
    scrollback
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
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
    // More specific than just "on main" to avoid filtering git command output
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
    fn count_lines() {
        assert_eq!(count_meaningful_lines("hello\nworld\n\n"), 2);
        assert_eq!(count_meaningful_lines(""), 0);
        assert_eq!(count_meaningful_lines("\n\n\n"), 0);
    }

    #[test]
    fn noise_detection_eval_error() {
        assert!(is_noise_line("(eval):4605: command not found: compdef", None));
    }

    #[test]
    fn noise_detection_starship_prompt() {
        assert!(is_noise_line("termbot on  main ()   [?] is 📦 v0.1.0 via 🦀 v1.76.0", None));
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
        assert!(!is_noise_line("drwxr-xr-x  5 user staff  160 Apr 11 src", None));
    }

    #[test]
    fn noise_detection_forge_line() {
        assert!(is_noise_line("# claude -p \"hello\"", Some("claude -p \"hello\"")));
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
        // "on main" in git output should NOT be filtered (only starship icon triggers it)
        assert!(!is_noise_line("On branch main", None));
        assert!(!is_noise_line("Switched to branch 'main'", None));
        assert!(!is_noise_line("Merge branch 'main' into feature", None));
    }

    #[test]
    fn empty_line_not_noise() {
        assert!(!is_noise_line("", None));
        assert!(!is_noise_line("  ", None));
    }

    #[test]
    fn multiline_count() {
        let text = "hello\n\n\nworld\n\n";
        assert_eq!(count_meaningful_lines(text), 2);
    }
}
