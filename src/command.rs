use anyhow::Result;
use regex::Regex;

#[derive(Debug, PartialEq)]
pub enum ParsedCommand {
    NewSession { name: String },
    Foreground { name: String },
    Background,
    ListSessions,
    KillSession { name: String },
    /// Send a prompt to Claude Code via the SDK (structured output, no terminal scraping)
    Claude { prompt: String },
    /// Capture and send the current terminal screen
    Screen,
    /// Enter interactive Claude mode — plain text routes to Claude instead of tmux
    ClaudeOn,
    /// Exit interactive Claude mode — plain text routes back to tmux
    ClaudeOff,
    ShellCommand { cmd: String },
    StdinInput { text: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Empty shell command — did you mean to type something after `: `?")]
    EmptyShellCommand,
    #[error("Missing session name")]
    MissingName,
    #[error("Session names can only contain letters, numbers, hyphens, and underscores")]
    InvalidSessionName,
    #[error("Session name too long (max 64 characters)")]
    SessionNameTooLong,
    #[error("Multi-line input is not supported — send one command at a time")]
    MultiLineNotAllowed,
}

const MAX_SESSION_NAME_LEN: usize = 64;

fn validate_session_name(name: &str) -> std::result::Result<(), ParseError> {
    if name.is_empty() {
        return Err(ParseError::MissingName);
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(ParseError::SessionNameTooLong);
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(ParseError::InvalidSessionName);
    }
    Ok(())
}

impl ParsedCommand {
    pub fn parse(input: &str) -> std::result::Result<Self, ParseError> {
        if let Some(rest) = input.strip_prefix(": ") {
            let rest = rest.trim();

            if rest.is_empty() {
                return Err(ParseError::EmptyShellCommand);
            }

            // Built-in commands
            if rest == "new" {
                return Err(ParseError::MissingName);
            }
            if let Some(name) = rest.strip_prefix("new ") {
                let name = name.trim();
                validate_session_name(name)?;
                return Ok(ParsedCommand::NewSession {
                    name: name.to_string(),
                });
            }

            if rest == "fg" {
                return Err(ParseError::MissingName);
            }
            if let Some(name) = rest.strip_prefix("fg ") {
                let name = name.trim();
                validate_session_name(name)?;
                return Ok(ParsedCommand::Foreground {
                    name: name.to_string(),
                });
            }

            if rest == "bg" {
                return Ok(ParsedCommand::Background);
            }

            if rest == "list" {
                return Ok(ParsedCommand::ListSessions);
            }

            if rest == "screen" {
                return Ok(ParsedCommand::Screen);
            }

            // `: claude on/off` — toggle interactive Claude mode
            if rest == "claude on" || rest == "claude" {
                return Ok(ParsedCommand::ClaudeOn);
            }
            if rest == "claude off" {
                return Ok(ParsedCommand::ClaudeOff);
            }
            // `: claude <prompt>` — one-shot Claude prompt
            if let Some(prompt) = rest.strip_prefix("claude ") {
                let prompt = prompt.trim();
                if !prompt.is_empty() {
                    return Ok(ParsedCommand::Claude {
                        prompt: prompt.to_string(),
                    });
                }
            }

            if rest == "kill" {
                return Err(ParseError::MissingName);
            }
            if let Some(name) = rest.strip_prefix("kill ") {
                let name = name.trim();
                validate_session_name(name)?;
                return Ok(ParsedCommand::KillSession {
                    name: name.to_string(),
                });
            }

            // Reject multi-line commands (newlines would execute as separate commands)
            if rest.contains('\n') {
                return Err(ParseError::MultiLineNotAllowed);
            }

            // Everything else after `: ` is a shell command
            Ok(ParsedCommand::ShellCommand {
                cmd: rest.to_string(),
            })
        } else {
            // No `: ` prefix — stdin input
            // Reject newlines (each line would execute as a separate shell command in tmux)
            if input.contains('\n') {
                return Err(ParseError::MultiLineNotAllowed);
            }
            Ok(ParsedCommand::StdinInput {
                text: input.to_string(),
            })
        }
    }
}

pub struct CommandBlocklist {
    patterns: Vec<Regex>,
}

impl CommandBlocklist {
    pub fn from_config(patterns: &[String]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for p in patterns {
            let regex = Regex::new(p)
                .map_err(|e| anyhow::anyhow!("Invalid blocklist pattern '{}': {}", p, e))?;
            compiled.push(regex);
        }
        Ok(Self { patterns: compiled })
    }

    /// Check if a command is blocked. Normalizes common evasion patterns
    /// (path prefixes, backslash insertion) before matching.
    pub fn is_blocked(&self, command: &str) -> bool {
        let normalized = Self::normalize_command(command);
        self.patterns.iter().any(|p| p.is_match(command) || p.is_match(&normalized))
    }

    /// Normalize common shell evasion patterns:
    /// - Strip common binary path prefixes (/usr/bin/, /bin/, /usr/sbin/, /sbin/)
    /// - Remove backslashes before non-special characters (su\do -> sudo)
    /// - Collapse multiple spaces
    /// - Merge and sort short flags for commands like `rm` so that
    ///   `rm -fr`, `rm -rf`, `rm -r -f` all normalize to `rm -fr`
    fn normalize_command(command: &str) -> String {
        let mut s = command.to_string();
        // Strip path prefixes
        for prefix in &["/usr/local/bin/", "/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/"] {
            s = s.replace(prefix, "");
        }
        // Remove backslashes before alphabetic characters (su\do -> sudo)
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    if next.is_alphabetic() {
                        continue; // skip the backslash
                    }
                }
            }
            result.push(c);
        }
        // Collapse multiple spaces
        let mut prev_space = false;
        let collapsed: String = result
            .chars()
            .filter(|&c| {
                if c == ' ' {
                    if prev_space { return false; }
                    prev_space = true;
                } else {
                    prev_space = false;
                }
                true
            })
            .collect();
        // Normalize short flags: merge consecutive flag groups and sort the
        // letters so that flag ordering cannot be used to evade patterns.
        // e.g. "rm -fr /" -> "rm -fr /", "rm -rf /" -> "rm -fr /",
        //      "rm -r -f /" -> "rm -fr /"
        Self::normalize_short_flags(&collapsed)
    }

    /// Merge consecutive short-flag arguments (tokens starting with `-` but
    /// not `--`) into a single flag token with letters sorted.
    ///
    /// `rm -r -f /`  → `rm -fr /`
    /// `rm -rf /`    → `rm -fr /`
    /// `rm -fr /`    → `rm -fr /`
    /// `rm --force --recursive /` → left as-is (long flags untouched)
    fn normalize_short_flags(command: &str) -> String {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        if tokens.is_empty() {
            return command.to_string();
        }

        let mut out: Vec<String> = Vec::with_capacity(tokens.len());
        let mut flag_chars: Vec<char> = Vec::new();

        for token in &tokens {
            if token.starts_with('-') && !token.starts_with("--") && token.len() > 1 {
                // Short flag group like -rf, -r, -f, -xvf …
                flag_chars.extend(token[1..].chars());
            } else {
                // Flush any accumulated flags first
                if !flag_chars.is_empty() {
                    flag_chars.sort_unstable();
                    flag_chars.dedup();
                    out.push(format!("-{}", flag_chars.iter().collect::<String>()));
                    flag_chars.clear();
                }
                out.push(token.to_string());
            }
        }
        // Flush trailing flags (unlikely but safe)
        if !flag_chars.is_empty() {
            flag_chars.sort_unstable();
            flag_chars.dedup();
            out.push(format!("-{}", flag_chars.iter().collect::<String>()));
        }

        out.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_session() {
        assert_eq!(
            ParsedCommand::parse(": new build").unwrap(),
            ParsedCommand::NewSession {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_foreground() {
        assert_eq!(
            ParsedCommand::parse(": fg build").unwrap(),
            ParsedCommand::Foreground {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_background() {
        assert_eq!(
            ParsedCommand::parse(": bg").unwrap(),
            ParsedCommand::Background
        );
    }

    #[test]
    fn parse_list() {
        assert_eq!(
            ParsedCommand::parse(": list").unwrap(),
            ParsedCommand::ListSessions
        );
    }

    #[test]
    fn parse_kill() {
        assert_eq!(
            ParsedCommand::parse(": kill build").unwrap(),
            ParsedCommand::KillSession {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_shell_command() {
        assert_eq!(
            ParsedCommand::parse(": ls -la").unwrap(),
            ParsedCommand::ShellCommand {
                cmd: "ls -la".into()
            }
        );
    }

    #[test]
    fn parse_stdin_input() {
        assert_eq!(
            ParsedCommand::parse("hello world").unwrap(),
            ParsedCommand::StdinInput {
                text: "hello world".into()
            }
        );
    }

    #[test]
    fn parse_claude_prompt() {
        assert_eq!(
            ParsedCommand::parse(": claude explain this code").unwrap(),
            ParsedCommand::Claude {
                prompt: "explain this code".into()
            }
        );
    }

    #[test]
    fn parse_empty_shell_command() {
        assert!(matches!(
            ParsedCommand::parse(":  "),
            Err(ParseError::EmptyShellCommand)
        ));
    }

    #[test]
    fn parse_missing_name() {
        assert!(matches!(
            ParsedCommand::parse(": new  "),
            Err(ParseError::MissingName)
        ));
    }

    #[test]
    fn blocklist_blocks_dangerous_commands() {
        let bl = CommandBlocklist::from_config(&[
            r"rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/".into(),
            r"sudo\s+".into(),
        ])
        .unwrap();
        assert!(bl.is_blocked("rm -rf /"));
        assert!(bl.is_blocked("rm -fr /"));
        assert!(bl.is_blocked("sudo reboot"));
        assert!(!bl.is_blocked("ls -la"));
        assert!(!bl.is_blocked("echo hello"));
    }

    #[test]
    fn blocklist_evasion_flag_reorder() {
        let bl = CommandBlocklist::from_config(&[r"rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/".into()]).unwrap();
        // Reversed flag order
        assert!(bl.is_blocked("rm -fr /"));
        assert!(bl.is_blocked("rm -rf /"));
        // Separate flags
        assert!(bl.is_blocked("rm -r -f /"));
        assert!(bl.is_blocked("rm -f -r /"));
        // Extra flags mixed in
        assert!(bl.is_blocked("rm -rfv /"));
        assert!(bl.is_blocked("rm -vfr /"));
        // Should NOT block without both flags
        assert!(!bl.is_blocked("rm -r /tmp/junk"));
        assert!(!bl.is_blocked("rm -f /tmp/junk"));
    }

    #[test]
    fn parse_claude_on() {
        assert_eq!(ParsedCommand::parse(": claude on").unwrap(), ParsedCommand::ClaudeOn);
        assert_eq!(ParsedCommand::parse(": claude").unwrap(), ParsedCommand::ClaudeOn);
    }

    #[test]
    fn parse_claude_off() {
        assert_eq!(ParsedCommand::parse(": claude off").unwrap(), ParsedCommand::ClaudeOff);
    }

    #[test]
    fn reject_invalid_session_name() {
        assert!(matches!(
            ParsedCommand::parse(": new foo:bar"),
            Err(ParseError::InvalidSessionName)
        ));
        assert!(matches!(
            ParsedCommand::parse(": new foo.bar"),
            Err(ParseError::InvalidSessionName)
        ));
        assert!(matches!(
            ParsedCommand::parse(": fg ../etc"),
            Err(ParseError::InvalidSessionName)
        ));
    }

    #[test]
    fn valid_session_names() {
        assert_eq!(
            ParsedCommand::parse(": new my-session_1").unwrap(),
            ParsedCommand::NewSession { name: "my-session_1".into() }
        );
    }

    #[test]
    fn reject_multiline_shell_command() {
        assert!(matches!(
            ParsedCommand::parse(": echo hello\nrm -rf /"),
            Err(ParseError::MultiLineNotAllowed)
        ));
    }

    #[test]
    fn reject_multiline_stdin() {
        assert!(matches!(
            ParsedCommand::parse("line1\nline2"),
            Err(ParseError::MultiLineNotAllowed)
        ));
    }

    #[test]
    fn reject_long_session_name() {
        let long_name = "a".repeat(65);
        assert!(matches!(
            ParsedCommand::parse(&format!(": new {}", long_name)),
            Err(ParseError::SessionNameTooLong)
        ));
    }

    #[test]
    fn blocklist_evasion_path_prefix() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("/usr/bin/sudo reboot"));
        assert!(bl.is_blocked("/bin/sudo reboot"));
    }

    #[test]
    fn blocklist_evasion_backslash() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked(r"su\do reboot"));
    }

    #[test]
    fn parse_screen() {
        assert_eq!(ParsedCommand::parse(": screen").unwrap(), ParsedCommand::Screen);
    }
}
