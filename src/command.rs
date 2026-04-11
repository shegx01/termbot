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
}

fn validate_session_name(name: &str) -> std::result::Result<(), ParseError> {
    if name.is_empty() {
        return Err(ParseError::MissingName);
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
                return Err(ParseError::EmptyShellCommand); // reuse error — multi-line not allowed
            }

            // Everything else after `: ` is a shell command
            Ok(ParsedCommand::ShellCommand {
                cmd: rest.to_string(),
            })
        } else {
            // No `: ` prefix — stdin input
            // Reject newlines (each line would execute as a separate shell command in tmux)
            if input.contains('\n') {
                return Err(ParseError::EmptyShellCommand);
            }
            Ok(ParsedCommand::StdinInput {
                text: input.to_string(),
            })
        }
    }
}

pub struct CommandBlocklist {
    patterns: Vec<Regex>,
    pattern_strings: Vec<String>,
}

impl CommandBlocklist {
    pub fn from_config(patterns: &[String]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for p in patterns {
            let regex = Regex::new(p)
                .map_err(|e| anyhow::anyhow!("Invalid blocklist pattern '{}': {}", p, e))?;
            compiled.push(regex);
        }
        Ok(Self {
            patterns: compiled,
            pattern_strings: patterns.to_vec(),
        })
    }

    pub fn is_blocked(&self, command: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(command))
    }

    pub fn matching_pattern(&self, command: &str) -> Option<&str> {
        for (i, pattern) in self.patterns.iter().enumerate() {
            if pattern.is_match(command) {
                return Some(&self.pattern_strings[i]);
            }
        }
        None
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
            r"rm\s+-rf\s+/".into(),
            r"sudo\s+".into(),
        ])
        .unwrap();
        assert!(bl.is_blocked("rm -rf /"));
        assert!(bl.is_blocked("sudo reboot"));
        assert!(!bl.is_blocked("ls -la"));
        assert!(!bl.is_blocked("echo hello"));
    }

    #[test]
    fn blocklist_matching_pattern() {
        let bl = CommandBlocklist::from_config(&[r"rm\s+-rf\s+/".into()]).unwrap();
        assert_eq!(bl.matching_pattern("rm -rf /"), Some(r"rm\s+-rf\s+/"));
        assert_eq!(bl.matching_pattern("ls"), None);
    }
}
