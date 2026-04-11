use anyhow::Result;
use regex::Regex;

#[derive(Debug, PartialEq)]
pub enum ParsedCommand {
    NewSession { name: String },
    Foreground { name: String },
    Background,
    ListSessions,
    KillSession { name: String },
    ShellCommand { cmd: String },
    StdinInput { text: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Empty shell command — did you mean to type something after `: `?")]
    EmptyShellCommand,
    #[error("Missing session name")]
    MissingName,
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
                if name.is_empty() {
                    return Err(ParseError::MissingName);
                }
                return Ok(ParsedCommand::NewSession {
                    name: name.to_string(),
                });
            }

            if rest == "fg" {
                return Err(ParseError::MissingName);
            }
            if let Some(name) = rest.strip_prefix("fg ") {
                let name = name.trim();
                if name.is_empty() {
                    return Err(ParseError::MissingName);
                }
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

            if rest == "kill" {
                return Err(ParseError::MissingName);
            }
            if let Some(name) = rest.strip_prefix("kill ") {
                let name = name.trim();
                if name.is_empty() {
                    return Err(ParseError::MissingName);
                }
                return Ok(ParsedCommand::KillSession {
                    name: name.to_string(),
                });
            }

            // Everything else after `: ` is a shell command
            Ok(ParsedCommand::ShellCommand {
                cmd: rest.to_string(),
            })
        } else {
            // No `: ` prefix — stdin input
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
