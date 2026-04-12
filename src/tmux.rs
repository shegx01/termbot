use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

pub struct TmuxClient {
    session_prefix: String,
}

impl TmuxClient {
    pub fn new() -> Self {
        Self {
            session_prefix: "tb-".into(),
        }
    }

    fn prefixed(&self, name: &str) -> String {
        format!("{}{}", self.session_prefix, name)
    }

    pub fn output_file_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/termbot-{}.out", name))
    }

    pub async fn create_session(&self, name: &str) -> Result<()> {
        let prefixed = self.prefixed(name);
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &prefixed])
            .status()
            .await
            .context("Failed to spawn tmux")?;
        if !status.success() {
            anyhow::bail!("tmux new-session failed for '{}'", name);
        }
        Ok(())
    }

    pub async fn kill_session(&self, name: &str) -> Result<()> {
        let prefixed = self.prefixed(name);
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &prefixed])
            .status()
            .await;
        // Clean up output file (legacy, in case it exists)
        let output_file = self.output_file_path(name);
        let _ = tokio::fs::remove_file(&output_file).await;
        Ok(())
    }

    pub async fn has_session(&self, name: &str) -> Result<bool> {
        let prefixed = self.prefixed(name);
        let status = Command::new("tmux")
            .args(["has-session", "-t", &prefixed])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("Failed to check tmux session")?;
        Ok(status.success())
    }

    /// Send a command to the session. Uses `-l` (literal) to prevent the shell
    /// from interpreting special characters like `?`, `*`, `!` as globs.
    /// Then sends Enter separately to submit the line.
    pub async fn send_keys(&self, name: &str, keys: &str) -> Result<()> {
        // Defense-in-depth: reject newlines at the tmux layer. Newlines in
        // send-keys -l would be interpreted as Enter, executing multiple commands.
        if keys.contains('\n') || keys.contains('\r') {
            anyhow::bail!("send_keys: refusing input containing newline characters");
        }
        let prefixed = self.prefixed(name);
        // Normalize smart/curly quotes from mobile keyboards (Telegram, Slack)
        // into plain ASCII quotes that shells understand
        let normalized = normalize_quotes(keys);
        // Send literal text (preserves ?, *, !, etc.)
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &prefixed, "-l", &normalized])
            .status()
            .await
            .context("Failed to send keys")?;
        if !status.success() {
            anyhow::bail!("tmux send-keys failed for '{}'", name);
        }
        // Send Enter to submit
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &prefixed, "Enter"])
            .status()
            .await
            .context("Failed to send Enter")?;
        if !status.success() {
            anyhow::bail!("tmux send-keys (Enter) failed for '{}'", name);
        }
        Ok(())
    }

    /// Send text as stdin to the session (literal + Enter). Same as send_keys
    /// but semantically distinct — used for plain text input to interactive programs.
    pub async fn send_stdin(&self, name: &str, text: &str) -> Result<()> {
        self.send_keys(name, text).await
    }

    /// Capture the current pane content as plain text (no ANSI escapes).
    /// Uses `-J` to join wrapped lines. Returns tmux's rendered screen grid.
    pub async fn capture_pane(&self, name: &str) -> Result<String> {
        let prefixed = self.prefixed(name);
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-J", "-t", &prefixed])
            .output()
            .await
            .context("Failed to capture pane")?;
        if !output.status.success() {
            anyhow::bail!("tmux capture-pane failed for '{}'", name);
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Capture pane content including scrollback history.
    /// `-S -` starts from the beginning of the scrollback buffer.
    pub async fn capture_pane_with_scrollback(&self, name: &str) -> Result<String> {
        let prefixed = self.prefixed(name);
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-J", "-S", "-", "-t", &prefixed])
            .output()
            .await
            .context("Failed to capture pane with scrollback")?;
        if !output.status.success() {
            anyhow::bail!("tmux capture-pane (scrollback) failed for '{}'", name);
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .await
            .context("Failed to list tmux sessions")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let sessions: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with(&self.session_prefix))
            .map(|line| line.trim_start_matches(&self.session_prefix).to_string())
            .collect();

        Ok(sessions)
    }
}

/// Normalize Unicode smart/curly quotes into plain ASCII equivalents.
/// Mobile keyboards (Telegram iOS/Android, Slack) auto-replace straight
/// quotes with typographic variants that shells don't recognize.
fn normalize_quotes(input: &str) -> String {
    input
        .replace(['\u{201C}', '\u{201D}'], "\"") // left/right double quotation marks ""
        .replace(['\u{2018}', '\u{2019}'], "'") // left/right single quotation marks ''
        .replace(['\u{00AB}', '\u{00BB}', '\u{2033}'], "\"") // angle quotes «» and double prime ″
        .replace('\u{2032}', "'") // prime ′
        .replace('\u{201A}', ",") // single low-9 quotation mark ‚
        .replace('\u{201E}', "\"") // double low-9 quotation mark „
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_curly_double_quotes() {
        assert_eq!(
            normalize_quotes("echo \u{201C}hello world\u{201D}"),
            "echo \"hello world\""
        );
    }

    #[test]
    fn normalize_curly_single_quotes() {
        assert_eq!(normalize_quotes("it\u{2019}s a test"), "it's a test");
    }

    #[test]
    fn normalize_angle_quotes() {
        assert_eq!(normalize_quotes("\u{00AB}bonjour\u{00BB}"), "\"bonjour\"");
    }

    #[test]
    fn passthrough_ascii_quotes() {
        let input = "echo \"hello\" 'world'";
        assert_eq!(normalize_quotes(input), input);
    }

    #[test]
    fn mixed_quotes() {
        assert_eq!(
            normalize_quotes("claude -p \u{201C}what is 2+2?\u{201D}"),
            "claude -p \"what is 2+2?\""
        );
    }
}
