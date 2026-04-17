use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

pub struct TmuxClient {
    session_prefix: String,
}

impl TmuxClient {
    pub fn new() -> Self {
        Self {
            session_prefix: "term-".into(),
        }
    }

    /// Check that `tmux` is on PATH and executable. Returns the version
    /// string on success, or an error describing why tmux is unavailable.
    pub async fn verify_available(&self) -> Result<String> {
        let output = Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .context("tmux binary not found — is tmux installed and on PATH?")?;
        if !output.status.success() {
            anyhow::bail!("tmux -V exited with status {}", output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn prefixed(&self, name: &str) -> String {
        format!("{}{}", self.session_prefix, name)
    }

    pub fn output_file_path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/terminus-{}.out", name))
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

    /// Send Ctrl+C (interrupt signal) to the session's foreground process.
    /// Uses `tmux send-keys C-c` which sends the interrupt without `-l` (literal).
    pub async fn send_interrupt(&self, name: &str) -> Result<()> {
        let prefixed = self.prefixed(name);
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &prefixed, "C-c"])
            .status()
            .await
            .context("Failed to send interrupt")?;
        if !status.success() {
            anyhow::bail!("tmux send-keys (C-c) failed for '{}'", name);
        }
        Ok(())
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

/// Normalize Unicode typographic characters that mobile keyboards
/// auto-replace (curly quotes, em-dashes) into plain ASCII equivalents.
/// Telegram iOS, Slack iOS, and macOS autocorrect all replace straight
/// quotes and `--` with typographic variants the shell-style parser
/// doesn't recognize — leaving `—name auth` to be treated as prompt text
/// instead of a flag.
///
/// Em-dash (U+2014) is normalized to `--` **only when it's in flag
/// position** — i.e. preceded by whitespace or at the start of input,
/// AND immediately followed by an ASCII letter. This matches the shape
/// of a long flag (`--name`, `--resume`, …) that iOS autocorrect
/// converted during typing, while leaving em-dashes used as prose
/// punctuation (e.g. `fix the bug—urgently` or `hello — world`)
/// untouched.
/// En-dash (U+2013) is left alone too: it's rarely an autocorrect
/// result and is commonly used legitimately in ranges ("pages 5–10").
pub(crate) fn normalize_quotes(input: &str) -> String {
    let quotes_fixed = input
        .replace(['\u{201C}', '\u{201D}'], "\"") // left/right double quotation marks ""
        .replace(['\u{2018}', '\u{2019}'], "'") // left/right single quotation marks ''
        .replace(['\u{00AB}', '\u{00BB}', '\u{2033}'], "\"") // angle quotes «» and double prime ″
        .replace('\u{2032}', "'") // prime ′
        .replace('\u{201A}', ",") // single low-9 quotation mark ‚
        .replace('\u{201E}', "\""); // double low-9 quotation mark „
                                    // Context-aware em-dash fold: only `<start|whitespace>—<ASCII letter>`.
    let mut out = String::with_capacity(quotes_fixed.len());
    let mut prev: Option<char> = None;
    let mut chars = quotes_fixed.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{2014}' {
            let at_flag_position = prev.is_none_or(char::is_whitespace)
                && matches!(chars.peek(), Some(n) if n.is_ascii_alphabetic());
            if at_flag_position {
                out.push_str("--");
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        prev = Some(c);
    }
    out
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

    #[test]
    fn normalize_em_dash_flag_prefix() {
        // iOS autocorrect turns `--name auth` into `—name auth`, which the
        // parser would otherwise treat as prompt text.
        assert_eq!(
            normalize_quotes("\u{2014}name auth fix bug"),
            "--name auth fix bug"
        );
    }

    #[test]
    fn normalize_em_dash_in_middle_of_input() {
        assert_eq!(
            normalize_quotes("claude on \u{2014}resume auth"),
            "claude on --resume auth"
        );
    }

    #[test]
    fn en_dash_is_not_normalized() {
        // En-dash (U+2013) is common in legitimate text ranges like
        // "pages 5–10" and is rarely an autocorrect result, so we leave it.
        let input = "pages 5\u{2013}10";
        assert_eq!(normalize_quotes(input), input);
    }

    #[test]
    fn em_dash_in_prose_is_preserved() {
        // A user typing `fix the bug—urgently` shouldn't have the em-dash
        // folded into `--` (that would make `--urgently` look like a flag
        // and error the prompt). Only em-dashes immediately followed by an
        // ASCII letter AND typed in flag position get folded.
        let input = "fix the bug\u{2014} urgently please";
        assert_eq!(normalize_quotes(input), input);
    }

    #[test]
    fn em_dash_between_words_with_spaces_is_preserved() {
        // Standard prose punctuation: "word — word". Space after em-dash
        // means no folding.
        let input = "hello \u{2014} world";
        assert_eq!(normalize_quotes(input), input);
    }

    #[test]
    fn em_dash_followed_by_digit_is_preserved() {
        // Ranges and numeric callouts typed with em-dash stay unmolested.
        let input = "chapter 3\u{2014}4";
        assert_eq!(normalize_quotes(input), input);
    }

    #[test]
    fn em_dash_glued_to_preceding_word_is_preserved() {
        // Prose: `fix the bug—urgently` keeps the em-dash because there's
        // no whitespace immediately before it, so it can't be in flag
        // position. This is the key prose case that mustn't be corrupted.
        assert_eq!(
            normalize_quotes("fix the bug\u{2014}urgently"),
            "fix the bug\u{2014}urgently"
        );
    }

    #[test]
    fn em_dash_at_start_of_input_with_letter_is_folded() {
        // This is exactly how the parser sees the flag portion of an input
        // like `: claude —resume foo` after the prefix is stripped.
        assert_eq!(normalize_quotes("\u{2014}resume foo"), "--resume foo");
    }
}
