use anyhow::Result;
use regex::Regex;

use crate::harness::HarnessKind;

#[derive(Debug, PartialEq)]
pub enum ParsedCommand {
    NewSession {
        name: String,
    },
    Foreground {
        name: String,
    },
    Background,
    ListSessions,
    KillSession {
        name: String,
    },
    /// Send a prompt to a harness (Claude, Gemini, Codex) via SDK
    HarnessPrompt {
        harness: HarnessKind,
        prompt: String,
    },
    /// Capture and send the current terminal screen
    Screen,
    /// Enter interactive harness mode — plain text routes to the harness
    HarnessOn {
        harness: HarnessKind,
    },
    /// Exit interactive harness mode — plain text routes back to tmux
    HarnessOff {
        harness: HarnessKind,
    },
    ShellCommand {
        cmd: String,
    },
    StdinInput {
        text: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Empty shell command — did you mean to type something after `{trigger} `?")]
    EmptyShellCommand { trigger: char },
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
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ParseError::InvalidSessionName);
    }
    Ok(())
}

impl ParsedCommand {
    pub fn parse(input: &str, trigger: char) -> std::result::Result<Self, ParseError> {
        if let Some(rest) = input
            .strip_prefix(trigger)
            .and_then(|s| s.strip_prefix(' '))
        {
            let rest = rest.trim();

            if rest.is_empty() {
                return Err(ParseError::EmptyShellCommand { trigger });
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

            // Harness commands: `: claude on/off`, `: gemini on/off`, `: codex <prompt>`, etc.
            // Bare `: claude` (no on/off/prompt) falls through to ShellCommand
            let first_word = rest.split_whitespace().next().unwrap_or("");
            if let Some(kind) = HarnessKind::from_str(first_word) {
                let after_harness = rest[first_word.len()..].trim();
                if after_harness == "on" {
                    return Ok(ParsedCommand::HarnessOn { harness: kind });
                }
                if after_harness == "off" {
                    return Ok(ParsedCommand::HarnessOff { harness: kind });
                }
                if !after_harness.is_empty() {
                    return Ok(ParsedCommand::HarnessPrompt {
                        harness: kind,
                        prompt: after_harness.to_string(),
                    });
                }
                // Bare `: claude` / `: gemini` falls through to ShellCommand
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

            // Reject multi-line shell commands — only single commands can be safely blocklist-checked
            if rest.contains('\n') || rest.contains('\r') {
                return Err(ParseError::MultiLineNotAllowed);
            }

            // Everything else after `: ` is a shell command
            Ok(ParsedCommand::ShellCommand {
                cmd: rest.to_string(),
            })
        } else {
            // No trigger prefix — stdin input (may contain newlines;
            // the dispatcher in app.rs decides whether to allow based on harness state)
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
    /// (path prefixes, backslash insertion) before matching. For multi-line
    /// payloads, each line is checked individually in addition to the whole string.
    #[must_use = "ignoring a blocklist check is a security risk"]
    pub fn is_blocked(&self, command: &str) -> bool {
        let normalized = Self::normalize_command(command);
        if self
            .patterns
            .iter()
            .any(|p| p.is_match(command) || p.is_match(&normalized))
        {
            return true;
        }
        // Also check each line individually (multi-line harness prompts could
        // embed dangerous commands on separate lines)
        if command.contains('\n') || command.contains('\r') {
            for line in command.split(['\n', '\r']) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let norm_line = Self::normalize_command(line);
                if self
                    .patterns
                    .iter()
                    .any(|p| p.is_match(line) || p.is_match(&norm_line))
                {
                    return true;
                }
            }
        }
        false
    }

    /// Normalize common shell evasion patterns:
    /// - Strip common binary path prefixes (/usr/bin/, /bin/, /usr/sbin/, /sbin/)
    /// - Remove backslashes before non-special characters (su\do -> sudo)
    /// - Collapse multiple spaces
    /// - Merge and sort short flags for commands like `rm` so that
    ///   `rm -fr`, `rm -rf`, `rm -r -f` all normalize to `rm -fr`
    fn normalize_command(command: &str) -> String {
        let mut s = command.to_string();
        // Strip null bytes and carriage returns that could confuse matching
        s.retain(|c| c != '\0' && c != '\r');
        // Strip path prefixes
        for prefix in &[
            "/usr/local/bin/",
            "/usr/bin/",
            "/usr/sbin/",
            "/bin/",
            "/sbin/",
        ] {
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
                    if prev_space {
                        return false;
                    }
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
            ParsedCommand::parse(": new build", ':').unwrap(),
            ParsedCommand::NewSession {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_foreground() {
        assert_eq!(
            ParsedCommand::parse(": fg build", ':').unwrap(),
            ParsedCommand::Foreground {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_background() {
        assert_eq!(
            ParsedCommand::parse(": bg", ':').unwrap(),
            ParsedCommand::Background
        );
    }

    #[test]
    fn parse_list() {
        assert_eq!(
            ParsedCommand::parse(": list", ':').unwrap(),
            ParsedCommand::ListSessions
        );
    }

    #[test]
    fn parse_kill() {
        assert_eq!(
            ParsedCommand::parse(": kill build", ':').unwrap(),
            ParsedCommand::KillSession {
                name: "build".into()
            }
        );
    }

    #[test]
    fn parse_shell_command() {
        assert_eq!(
            ParsedCommand::parse(": ls -la", ':').unwrap(),
            ParsedCommand::ShellCommand {
                cmd: "ls -la".into()
            }
        );
    }

    #[test]
    fn parse_stdin_input() {
        assert_eq!(
            ParsedCommand::parse("hello world", ':').unwrap(),
            ParsedCommand::StdinInput {
                text: "hello world".into()
            }
        );
    }

    #[test]
    fn parse_claude_prompt() {
        assert_eq!(
            ParsedCommand::parse(": claude explain this code", ':').unwrap(),
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Claude,
                prompt: "explain this code".into()
            }
        );
    }

    #[test]
    fn parse_empty_shell_command() {
        assert!(matches!(
            ParsedCommand::parse(":  ", ':'),
            Err(ParseError::EmptyShellCommand { .. })
        ));
    }

    #[test]
    fn parse_missing_name() {
        assert!(matches!(
            ParsedCommand::parse(": new  ", ':'),
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
        let bl =
            CommandBlocklist::from_config(&[r"rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/".into()]).unwrap();
        assert!(bl.is_blocked("rm -fr /"));
        assert!(bl.is_blocked("rm -rf /"));
        assert!(bl.is_blocked("rm -r -f /"));
        assert!(bl.is_blocked("rm -f -r /"));
        assert!(bl.is_blocked("rm -rfv /"));
        assert!(bl.is_blocked("rm -vfr /"));
        assert!(!bl.is_blocked("rm -r /tmp/junk"));
        assert!(!bl.is_blocked("rm -f /tmp/junk"));
    }

    #[test]
    fn parse_claude_on() {
        assert_eq!(
            ParsedCommand::parse(": claude on", ':').unwrap(),
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
            }
        );
    }

    #[test]
    fn parse_bare_claude_is_shell_command() {
        assert_eq!(
            ParsedCommand::parse(": claude", ':').unwrap(),
            ParsedCommand::ShellCommand {
                cmd: "claude".into()
            }
        );
    }

    #[test]
    fn parse_claude_off() {
        assert_eq!(
            ParsedCommand::parse(": claude off", ':').unwrap(),
            ParsedCommand::HarnessOff {
                harness: HarnessKind::Claude,
            }
        );
    }

    #[test]
    fn parse_gemini_on() {
        assert_eq!(
            ParsedCommand::parse(": gemini on", ':').unwrap(),
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Gemini,
            }
        );
    }

    #[test]
    fn parse_gemini_off() {
        assert_eq!(
            ParsedCommand::parse(": gemini off", ':').unwrap(),
            ParsedCommand::HarnessOff {
                harness: HarnessKind::Gemini,
            }
        );
    }

    #[test]
    fn parse_codex_prompt() {
        assert_eq!(
            ParsedCommand::parse(": codex explain this", ':').unwrap(),
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Codex,
                prompt: "explain this".into()
            }
        );
    }

    #[test]
    fn parse_bare_gemini_is_shell_command() {
        assert_eq!(
            ParsedCommand::parse(": gemini", ':').unwrap(),
            ParsedCommand::ShellCommand {
                cmd: "gemini".into()
            }
        );
    }

    #[test]
    fn reject_invalid_session_name() {
        assert!(matches!(
            ParsedCommand::parse(": new foo:bar", ':'),
            Err(ParseError::InvalidSessionName)
        ));
        assert!(matches!(
            ParsedCommand::parse(": new foo.bar", ':'),
            Err(ParseError::InvalidSessionName)
        ));
        assert!(matches!(
            ParsedCommand::parse(": fg ../etc", ':'),
            Err(ParseError::InvalidSessionName)
        ));
    }

    #[test]
    fn valid_session_names() {
        assert_eq!(
            ParsedCommand::parse(": new my-session_1", ':').unwrap(),
            ParsedCommand::NewSession {
                name: "my-session_1".into()
            }
        );
    }

    #[test]
    fn multiline_stdin_allowed_by_parser() {
        // Parser allows multi-line StdinInput; dispatcher decides based on harness state
        assert_eq!(
            ParsedCommand::parse("line1\nline2", ':').unwrap(),
            ParsedCommand::StdinInput {
                text: "line1\nline2".into()
            }
        );
    }

    #[test]
    fn reject_long_session_name() {
        let long_name = "a".repeat(65);
        assert!(matches!(
            ParsedCommand::parse(&format!(": new {}", long_name), ':'),
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
        assert_eq!(
            ParsedCommand::parse(": screen", ':').unwrap(),
            ParsedCommand::Screen
        );
    }

    // --- New tests for configurable trigger and multi-line ---

    #[test]
    fn parse_custom_trigger() {
        assert_eq!(
            ParsedCommand::parse("! ls -la", '!').unwrap(),
            ParsedCommand::ShellCommand {
                cmd: "ls -la".into()
            }
        );
    }

    #[test]
    fn parse_custom_trigger_harness_prompt() {
        assert_eq!(
            ParsedCommand::parse("! claude explain this", '!').unwrap(),
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Claude,
                prompt: "explain this".into()
            }
        );
    }

    #[test]
    fn parse_multiline_harness_prompt_allowed() {
        assert_eq!(
            ParsedCommand::parse(": claude explain\nthis code", ':').unwrap(),
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Claude,
                prompt: "explain\nthis code".into()
            }
        );
    }

    #[test]
    fn parse_multiline_shell_command_rejected() {
        assert!(matches!(
            ParsedCommand::parse(": echo hello\nrm -rf /", ':'),
            Err(ParseError::MultiLineNotAllowed)
        ));
    }

    #[test]
    fn parse_wrong_trigger_is_stdin() {
        // ":" prefix with trigger "!" is not recognized as a command
        assert_eq!(
            ParsedCommand::parse(": ls", '!').unwrap(),
            ParsedCommand::StdinInput {
                text: ": ls".into()
            }
        );
    }

    #[test]
    fn empty_shell_command_includes_trigger() {
        let err = ParsedCommand::parse(":  ", ':').unwrap_err();
        assert!(err.to_string().contains(": "));
        let err = ParsedCommand::parse("!  ", '!').unwrap_err();
        assert!(err.to_string().contains("! "));
    }

    #[test]
    fn trigger_without_space_is_stdin() {
        assert_eq!(
            ParsedCommand::parse(":ls", ':').unwrap(),
            ParsedCommand::StdinInput { text: ":ls".into() }
        );
    }

    #[test]
    fn blocklist_per_line_multiline() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("innocent text\nsudo reboot"));
        assert!(bl.is_blocked("line1\nline2\nsudo rm -rf /"));
        assert!(!bl.is_blocked("innocent text\nls -la")); // no dangerous commands on any line
    }

    #[test]
    fn blocklist_normalize_strips_cr_and_null() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("su\0do reboot")); // null stripped -> "sudo reboot"
        assert!(bl.is_blocked("sudo\r reboot")); // \r stripped -> "sudo reboot"
    }
}
