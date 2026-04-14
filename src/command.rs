use std::path::PathBuf;

use anyhow::Result;
use regex::Regex;

use crate::harness::HarnessKind;
use crate::tmux::normalize_quotes;

/// CLI-style options that can be passed with `: claude on [options]`.
///
/// These options persist for the duration of the harness session (until
/// `: claude off`) and are applied to every prompt sent in that session.
///
/// Only options that make sense for subscription-based Claude Code are
/// included — billing options like `--max-budget-usd` are intentionally
/// omitted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HarnessOptions {
    /// Model override (e.g. "sonnet", "opus").
    pub model: Option<String>,
    /// Thinking effort level: low, medium, high, max.
    pub effort: Option<String>,
    /// Custom system prompt (replaces default).
    pub system_prompt: Option<String>,
    /// Text appended to the default system prompt.
    pub append_system_prompt: Option<String>,
    /// Additional directories for context.
    pub add_dirs: Vec<PathBuf>,
    /// Maximum agentic turns per prompt.
    pub max_turns: Option<u32>,
    /// Path to a Claude Code settings file or inline JSON.
    pub settings: Option<String>,
    /// Path to an MCP server config file.
    pub mcp_config: Option<PathBuf>,
}

impl HarnessOptions {
    /// Returns true if no options were set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.effort.is_none()
            && self.system_prompt.is_none()
            && self.append_system_prompt.is_none()
            && self.add_dirs.is_empty()
            && self.max_turns.is_none()
            && self.settings.is_none()
            && self.mcp_config.is_none()
    }

    /// Build a human-readable summary of active options for the ON confirmation message.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref m) = self.model {
            parts.push(format!("model={}", m));
        }
        if let Some(ref e) = self.effort {
            parts.push(format!("effort={}", e));
        }
        if let Some(ref sp) = self.system_prompt {
            let preview: String = sp.chars().take(30).collect();
            let suffix = if sp.chars().count() > 30 { "..." } else { "" };
            parts.push(format!("system-prompt=\"{}{}\"", preview, suffix));
        }
        if let Some(ref asp) = self.append_system_prompt {
            let preview: String = asp.chars().take(30).collect();
            let suffix = if asp.chars().count() > 30 { "..." } else { "" };
            parts.push(format!("append-system-prompt=\"{}{}\"", preview, suffix));
        }
        for d in &self.add_dirs {
            parts.push(format!("add-dir={}", d.display()));
        }
        if let Some(n) = self.max_turns {
            parts.push(format!("max-turns={}", n));
        }
        if let Some(ref s) = self.settings {
            parts.push(format!("settings={}", s));
        }
        if let Some(ref m) = self.mcp_config {
            parts.push(format!("mcp-config={}", m.display()));
        }
        parts.join(", ")
    }
}

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
        options: HarnessOptions,
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
    #[error("Invalid harness option: {0}")]
    InvalidHarnessOption(String),
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
                    return Ok(ParsedCommand::HarnessOn {
                        harness: kind,
                        options: HarnessOptions::default(),
                    });
                }
                // `: claude on --model sonnet --add-dir ../lib`
                if let Some(on_args) = after_harness.strip_prefix("on ") {
                    let options = parse_harness_options(on_args.trim())?;
                    return Ok(ParsedCommand::HarnessOn {
                        harness: kind,
                        options,
                    });
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

/// Parse CLI-style options from the text after `on `.
///
/// Supported flags:
///   --model <name>                  Model override
///   --effort <low|medium|high|max>  Thinking effort level
///   --system-prompt <text>          Replace default system prompt
///   --append-system-prompt <text>   Append to default system prompt
///   --add-dir <path>                Additional context directory (repeatable)
///   --max-turns <n>                 Maximum agentic turns per prompt
///   --settings <path|json>          Claude Code settings file or inline JSON
///   --mcp-config <path>             MCP server config file
fn parse_harness_options(input: &str) -> std::result::Result<HarnessOptions, ParseError> {
    let mut opts = HarnessOptions::default();
    // Normalize smart/curly quotes from mobile keyboards before tokenizing
    let normalized = normalize_quotes(input);
    let tokens = shell_tokenize(&normalized);
    let mut i = 0;

    while i < tokens.len() {
        // Support --flag=value syntax: split on first '=' if present
        let (flag, eq_value) = if tokens[i].starts_with("--") {
            if let Some(idx) = tokens[i].find('=') {
                (&tokens[i][..idx], Some(tokens[i][idx + 1..].to_string()))
            } else {
                (tokens[i].as_str(), None)
            }
        } else {
            (tokens[i].as_str(), None)
        };
        // Macro-like helper: use =value if present, otherwise consume next token
        macro_rules! get_value {
            () => {
                match eq_value {
                    Some(ref v) if !v.is_empty() => v.clone(),
                    Some(_) => {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "{}= requires a value",
                            flag
                        )));
                    }
                    None => take_value(&tokens, &mut i, flag)?,
                }
            };
        }
        match flag {
            "--model" | "-m" => {
                let val = get_value!();
                opts.model = Some(val);
            }
            "--effort" | "-e" => {
                let val = get_value!();
                match val.to_lowercase().as_str() {
                    "low" | "medium" | "high" | "max" => {
                        opts.effort = Some(val.to_lowercase());
                    }
                    _ => {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "Invalid effort level '{}' — expected low, medium, high, or max",
                            val
                        )));
                    }
                }
            }
            "--system-prompt" => {
                let val = get_value!();
                opts.system_prompt = Some(val);
            }
            "--append-system-prompt" => {
                let val = get_value!();
                opts.append_system_prompt = Some(val);
            }
            "--add-dir" | "-d" => {
                let val = get_value!();
                opts.add_dirs.push(PathBuf::from(val));
            }
            "--max-turns" | "-n" => {
                let val = get_value!();
                let n: u32 = val.parse().map_err(|_| {
                    ParseError::InvalidHarnessOption(format!(
                        "Invalid --max-turns value '{}' — expected a positive integer",
                        val
                    ))
                })?;
                if n == 0 {
                    return Err(ParseError::InvalidHarnessOption(
                        "--max-turns must be at least 1".to_string(),
                    ));
                }
                opts.max_turns = Some(n);
            }
            "--settings" => {
                let val = get_value!();
                opts.settings = Some(val);
            }
            "--mcp-config" => {
                let val = get_value!();
                opts.mcp_config = Some(PathBuf::from(val));
            }
            other => {
                // Better error for short-flag=value syntax (e.g. `-m=opus`)
                if other.starts_with('-') && other.contains('=') {
                    let key = other.split('=').next().unwrap_or(other);
                    return Err(ParseError::InvalidHarnessOption(format!(
                        "'=' syntax is only supported with long flags (use '{} value' instead of '{}')",
                        key, other
                    )));
                }
                return Err(ParseError::InvalidHarnessOption(format!(
                    "Unknown option '{}'. Supported: --model, --effort, --system-prompt, --append-system-prompt, --add-dir, --max-turns, --settings, --mcp-config",
                    other
                )));
            }
        }
        i += 1;
    }

    // Reject mutually exclusive options
    if opts.system_prompt.is_some() && opts.append_system_prompt.is_some() {
        return Err(ParseError::InvalidHarnessOption(
            "Cannot use both --system-prompt and --append-system-prompt".to_string(),
        ));
    }

    Ok(opts)
}

/// Extract the value after a flag, advancing the index.
/// Rejects another flag (starts with `-`) as a value to catch cases like
/// `--model --effort high` where the user forgot to provide the model name.
fn take_value(
    tokens: &[String],
    i: &mut usize,
    flag: &str,
) -> std::result::Result<String, ParseError> {
    if *i + 1 >= tokens.len() {
        return Err(ParseError::InvalidHarnessOption(format!(
            "{} requires a value",
            flag
        )));
    }
    let next = &tokens[*i + 1];
    if next.starts_with('-') && next.len() > 1 {
        return Err(ParseError::InvalidHarnessOption(format!(
            "{} requires a value, but got '{}' (another flag?)",
            flag, next
        )));
    }
    *i += 1;
    Ok(tokens[*i].clone())
}

/// Simple shell-like tokenizer that respects double and single quotes.
/// Does not handle escapes inside quotes — good enough for chat input.
fn shell_tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in input.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
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
                options: HarnessOptions::default(),
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
                options: HarnessOptions::default(),
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

    // ── Harness options parsing ─────────────────────────────────────────────

    #[test]
    fn parse_claude_on_with_model() {
        let cmd = ParsedCommand::parse(": claude on --model sonnet", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("sonnet".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_short_model_flag() {
        let cmd = ParsedCommand::parse(": claude on -m opus", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("opus".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_effort() {
        let cmd = ParsedCommand::parse(": claude on --effort high", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    effort: Some("high".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_invalid_effort_rejected() {
        let err = ParsedCommand::parse(": claude on --effort turbo", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("turbo"));
    }

    #[test]
    fn parse_claude_on_with_add_dir() {
        let cmd = ParsedCommand::parse(": claude on --add-dir ../lib", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    add_dirs: vec![PathBuf::from("../lib")],
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_multiple_add_dirs() {
        let cmd =
            ParsedCommand::parse(": claude on --add-dir ../lib --add-dir ../shared", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    add_dirs: vec![PathBuf::from("../lib"), PathBuf::from("../shared")],
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_max_turns() {
        let cmd = ParsedCommand::parse(": claude on -n 5", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    max_turns: Some(5),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_invalid_max_turns_rejected() {
        let err = ParsedCommand::parse(": claude on --max-turns abc", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
    }

    #[test]
    fn parse_claude_on_with_system_prompt() {
        let cmd =
            ParsedCommand::parse(": claude on --system-prompt \"You are a Rust expert\"", ':')
                .unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    system_prompt: Some("You are a Rust expert".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_multiple_options() {
        let cmd = ParsedCommand::parse(
            ": claude on --model sonnet --effort high --add-dir ../lib -n 10",
            ':',
        )
        .unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("sonnet".into()),
                    effort: Some("high".into()),
                    add_dirs: vec![PathBuf::from("../lib")],
                    max_turns: Some(10),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_unknown_option_rejected() {
        let err = ParsedCommand::parse(": claude on --max-budget-usd 5", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Unknown option"));
    }

    #[test]
    fn parse_claude_on_missing_value_rejected() {
        let err = ParsedCommand::parse(": claude on --model", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn parse_claude_on_with_settings() {
        let cmd = ParsedCommand::parse(": claude on --settings ./settings.json", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    settings: Some("./settings.json".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_mcp_config() {
        let cmd = ParsedCommand::parse(": claude on --mcp-config ./mcp.json", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    mcp_config: Some(PathBuf::from("./mcp.json")),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_with_append_system_prompt() {
        let cmd = ParsedCommand::parse(
            ": claude on --append-system-prompt 'Always use TypeScript'",
            ':',
        )
        .unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    append_system_prompt: Some("Always use TypeScript".into()),
                    ..Default::default()
                },
            }
        );
    }

    // ── shell_tokenize ──────────────────────────────────────────────────────

    #[test]
    fn shell_tokenize_simple_words() {
        assert_eq!(shell_tokenize("--model sonnet"), vec!["--model", "sonnet"]);
    }

    #[test]
    fn shell_tokenize_double_quoted_string() {
        assert_eq!(
            shell_tokenize("--system-prompt \"You are a Rust expert\""),
            vec!["--system-prompt", "You are a Rust expert"]
        );
    }

    #[test]
    fn shell_tokenize_single_quoted_string() {
        assert_eq!(
            shell_tokenize("--system-prompt 'Be concise'"),
            vec!["--system-prompt", "Be concise"]
        );
    }

    #[test]
    fn shell_tokenize_mixed_quoted_and_plain() {
        assert_eq!(
            shell_tokenize("--model opus --system-prompt \"Be helpful\" --add-dir ../lib"),
            vec![
                "--model",
                "opus",
                "--system-prompt",
                "Be helpful",
                "--add-dir",
                "../lib"
            ]
        );
    }

    #[test]
    fn shell_tokenize_empty_input_returns_empty() {
        let result: Vec<String> = shell_tokenize("");
        assert!(result.is_empty());
    }

    // ── HarnessOptions::summary ─────────────────────────────────────────────

    #[test]
    fn harness_options_summary_empty() {
        assert_eq!(HarnessOptions::default().summary(), "");
    }

    #[test]
    fn harness_options_summary_model_and_effort() {
        let opts = HarnessOptions {
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            ..Default::default()
        };
        assert_eq!(opts.summary(), "model=sonnet, effort=high");
    }

    #[test]
    fn harness_options_is_empty_true_for_default() {
        assert!(HarnessOptions::default().is_empty());
    }

    #[test]
    fn harness_options_is_empty_false_when_model_set() {
        let opts = HarnessOptions {
            model: Some("sonnet".into()),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    // ── Round 1 bug-fix tests ───────────────────────────────────────────────

    #[test]
    fn parse_claude_on_key_equals_value_syntax() {
        let cmd = ParsedCommand::parse(": claude on --model=sonnet", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("sonnet".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_mixed_equals_and_space_syntax() {
        let cmd = ParsedCommand::parse(": claude on --model=opus --effort high", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("opus".into()),
                    effort: Some("high".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_max_turns_zero_rejected() {
        let err = ParsedCommand::parse(": claude on --max-turns 0", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn parse_claude_on_smart_quotes_normalized() {
        // Curly double quotes: \u{201C} and \u{201D}
        let input = format!(
            ": claude on --system-prompt {}Be concise{}",
            '\u{201C}', '\u{201D}'
        );
        let cmd = ParsedCommand::parse(&input, ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    system_prompt: Some("Be concise".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn parse_claude_on_smart_single_quotes_normalized() {
        // Curly single quotes: \u{2018} and \u{2019}
        let input = format!(
            ": claude on --system-prompt {}Be concise{}",
            '\u{2018}', '\u{2019}'
        );
        let cmd = ParsedCommand::parse(&input, ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    system_prompt: Some("Be concise".into()),
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn harness_options_summary_truncates_long_system_prompt() {
        let long_prompt = "a".repeat(50);
        let opts = HarnessOptions {
            system_prompt: Some(long_prompt),
            ..Default::default()
        };
        let summary = opts.summary();
        assert!(summary.contains("..."));
        // The preview should be exactly 30 chars of 'a'
        assert!(summary.contains(&"a".repeat(30)));
    }

    // ── Round 2 bug-fix tests ───────────────────────────────────────────────

    #[test]
    fn parse_claude_on_empty_equals_value_rejected() {
        let err = ParsedCommand::parse(": claude on --model=", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn parse_claude_on_short_flag_equals_syntax_rejected_with_hint() {
        let err = ParsedCommand::parse(": claude on -m=opus", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("long flags"));
    }

    #[test]
    fn parse_claude_on_both_system_prompts_rejected() {
        let err = ParsedCommand::parse(
            ": claude on --system-prompt \"A\" --append-system-prompt \"B\"",
            ':',
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Cannot use both"));
    }

    #[test]
    fn parse_claude_on_flag_consumed_as_value_rejected() {
        // User forgot the model name — the next flag shouldn't be consumed as value
        let err = ParsedCommand::parse(": claude on --model --effort high", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("another flag"));
    }

    #[test]
    fn shell_tokenize_unterminated_quote_captures_rest() {
        // Unterminated quote greedily captures the remainder
        assert_eq!(
            shell_tokenize("--system-prompt \"hello world"),
            vec!["--system-prompt", "hello world"]
        );
    }
}
