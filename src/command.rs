use std::path::PathBuf;

use anyhow::Result;
use regex::Regex;

use crate::harness::HarnessKind;
use crate::tmux::normalize_quotes;

/// Opencode-only CLI subcommand for read-only passthrough.
///
/// Only emitted for the opencode harness; other harnesses fall through to
/// their current parse paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpencodeSubcommand {
    /// `opencode models [provider]` — list available models.
    Models,
    /// `opencode stats [--days N] [--tools] [--models] [--project]` — usage stats.
    Stats,
    /// `opencode session list [--max-count N] [--format ...]` — list sessions.
    Sessions,
    /// `opencode auth list` — list configured providers. User-facing keyword: `providers`.
    Providers,
    /// `opencode export <sessionID>` — export one session as JSON.
    Export,
}

/// Chat-safe subcommand surfaces for the gemini harness. Gemini-cli's real
/// "subcommands" (`mcp`, `extensions`, `skills`, `hooks`) are all interactive
/// or destructive; the only safe surfaces are the read-only `--list-*` flags
/// which we expose under user-friendly chat keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiSubcommand {
    /// `gemini --list-sessions` — list saved sessions for the current project.
    /// User-facing keyword: `sessions`.
    Sessions,
    /// `gemini --list-extensions` — list installed extensions.
    /// User-facing keyword: `extensions`.
    Extensions,
}

/// Chat-safe codex subcommand surfaces. Codex 0.128 has only one read-only
/// top-level surface (`resume --all` exposed as `sessions`), one direct-mutation
/// surface (`apply <task_id>`), and the experimental `cloud` subgroup (5
/// single-shot subcommands). Everything else (`login`, `mcp`, `app`, etc.) is
/// blocked at the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSubcommand {
    /// `codex resume --all` — list saved sessions for this project.
    /// User-facing keyword: `sessions`.
    Sessions,
    /// `codex apply <task_id>` — apply a Codex Cloud task diff to the working
    /// tree as a `git apply`. Top-level shortcut for `codex cloud apply`.
    Apply,
    /// `codex cloud <sub>` — Codex Cloud subgroup.
    Cloud(CodexCloudSubcommand),
}

/// Subcommands of the experimental `codex cloud` surface. Each is single-shot
/// (returns immediately) — the "submit then apply" lifecycle is just two
/// independent CLI calls from the user's perspective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCloudSubcommand {
    /// `codex cloud list` — list cloud tasks.
    List,
    /// `codex cloud status <task_id>` — show task status.
    Status,
    /// `codex cloud diff <task_id>` — show task diff.
    Diff,
    /// `codex cloud apply <task_id>` — apply task diff locally.
    Apply,
    /// `codex cloud exec --env <env_id> <query>` — submit a new cloud task.
    Exec,
}

/// Sum type over per-harness subcommand enums. Carried in
/// [`ParsedCommand::HarnessSubcommand`] so the dispatcher can route to the
/// right harness without losing per-harness type safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSubcommandKind {
    Opencode(OpencodeSubcommand),
    Gemini(GeminiSubcommand),
    Codex(CodexSubcommand),
}

/// Per-harness extras for [`HarnessOptions`]. Each variant carries the
/// harness-specific flags that don't apply to other harnesses.
///
/// A `None` value on `HarnessOptions::extras` means no harness-specific
/// flags were supplied; the harness consumer should fall back to its own
/// `[harness.<name>]` config for defaults.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessExtras {
    Claude(ClaudeExtras),
    Codex(CodexExtras),
    Gemini(GeminiExtras),
    Opencode(OpencodeExtras),
}

/// Claude-only flags (thinking, custom system prompts, settings, MCP, permissions, max-turns).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeExtras {
    /// Thinking effort level: low, medium, high, max.
    pub effort: Option<String>,
    /// Custom system prompt (replaces default).
    pub system_prompt: Option<String>,
    /// Text appended to the default system prompt.
    pub append_system_prompt: Option<String>,
    /// Maximum agentic turns per prompt.
    pub max_turns: Option<u32>,
    /// Path to a Claude Code settings file or inline JSON.
    pub settings: Option<String>,
    /// Path to an MCP server config file.
    pub mcp_config: Option<PathBuf>,
    /// Permission mode: default, acceptEdits, plan, bypassPermissions.
    pub permission_mode: Option<String>,
}

impl ClaudeExtras {
    /// `true` iff every field is `None` (no flags supplied). Preferred over
    /// `*self == ClaudeExtras::default()` because it is robust to a future
    /// field whose `Default` value is meaningful (e.g. `bool` defaulting to
    /// `false` would otherwise silently misfire the comparison).
    pub fn is_empty(&self) -> bool {
        self.effort.is_none()
            && self.system_prompt.is_none()
            && self.append_system_prompt.is_none()
            && self.max_turns.is_none()
            && self.settings.is_none()
            && self.mcp_config.is_none()
            && self.permission_mode.is_none()
    }
}

/// Codex-only flags (`--sandbox`, `--profile`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexExtras {
    /// `--sandbox <read-only|workspace-write|danger-full-access>`. Overrides
    /// `[harness.codex].sandbox` when set.
    pub sandbox: Option<String>,
    /// `--profile <name>`. Selects a named profile from `~/.codex/config.toml`.
    /// Overrides `[harness.codex].profile`.
    pub profile: Option<String>,
}

impl CodexExtras {
    /// `true` iff every field is `None`.
    pub fn is_empty(&self) -> bool {
        self.sandbox.is_none() && self.profile.is_none()
    }
}

/// Gemini-only flags (`--approval-mode`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeminiExtras {
    /// `--approval-mode <default|auto_edit|yolo|plan>`. Overrides
    /// `[harness.gemini].approval_mode` when set.
    pub approval_mode: Option<String>,
}

impl GeminiExtras {
    /// `true` iff every field is `None`.
    pub fn is_empty(&self) -> bool {
        self.approval_mode.is_none()
    }
}

/// Opencode-only flags (`--agent`, `--title`, `--share`, `--pure`, `--fork`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpencodeExtras {
    /// Override the agent (`--agent <name>`). e.g., "build" to enable tool-use.
    pub agent: Option<String>,
    /// Human-readable session title (`--title`).
    pub title: Option<String>,
    /// Share flag — opencode generates a share URL for the session.
    pub share: bool,
    /// Pure mode — opencode runs without external plugins.
    pub pure: bool,
    /// Fork the session before continuing (requires --continue or --resume).
    pub fork: bool,
}

impl OpencodeExtras {
    /// `true` iff every field is at its "no flag" value (None for Options,
    /// false for booleans).
    pub fn is_empty(&self) -> bool {
        self.agent.is_none() && self.title.is_none() && !self.share && !self.pure && !self.fork
    }
}

/// CLI-style options that can be passed with `: <harness> [options] [prompt]`
/// or `: <harness> on [options]`.
///
/// Shared fields are flat on this struct; per-harness fields live inside
/// [`HarnessExtras`] so a flag that only makes sense for one harness can't
/// silently leak into a prompt for another. The parser routes flags into the
/// right extras variant based on the harness kind, and rejects cross-harness
/// flag misuse with a chat-safe error.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HarnessOptions {
    /// Model override (e.g. "sonnet", "opus").
    pub model: Option<String>,
    /// Additional directories for context (claude `--add-dir`, codex `--add-dir`).
    pub add_dirs: Vec<PathBuf>,
    /// Schema name for structured output (registered in terminus.toml).
    /// Used by claude (full webhook delivery) and codex (registered names also
    /// trigger webhook delivery; inline-JSON / file-path forms render to chat
    /// only). Other harnesses reject `--schema` at run time with a redirect
    /// error.
    pub schema: Option<String>,
    /// Named session for multi-turn resume (`--name` / `-n`).
    pub name: Option<String>,
    /// Strict resume of a named session (`--resume` / `--continue <name>`).
    pub resume: Option<String>,
    /// Bare `--continue`: continue the most recent session for harnesses that
    /// support it (opencode, gemini, codex). Distinguished from `resume`
    /// (name-based resume).
    pub continue_last: bool,
    /// Per-harness flags. `None` when no harness-specific flags were set;
    /// otherwise one of [`ClaudeExtras`], [`CodexExtras`], [`GeminiExtras`],
    /// [`OpencodeExtras`].
    pub extras: Option<HarnessExtras>,
}

impl HarnessOptions {
    /// Returns true if no options were set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.add_dirs.is_empty()
            && self.schema.is_none()
            && self.name.is_none()
            && self.resume.is_none()
            && !self.continue_last
            && self.extras.is_none()
    }

    /// View into per-harness Claude flags. Returns `None` for any other extras
    /// variant — harness consumers should fall back to their own config defaults.
    pub fn claude_extras(&self) -> Option<&ClaudeExtras> {
        match &self.extras {
            Some(HarnessExtras::Claude(e)) => Some(e),
            _ => None,
        }
    }

    /// View into per-harness Codex flags.
    pub fn codex_extras(&self) -> Option<&CodexExtras> {
        match &self.extras {
            Some(HarnessExtras::Codex(e)) => Some(e),
            _ => None,
        }
    }

    /// View into per-harness Gemini flags.
    pub fn gemini_extras(&self) -> Option<&GeminiExtras> {
        match &self.extras {
            Some(HarnessExtras::Gemini(e)) => Some(e),
            _ => None,
        }
    }

    /// View into per-harness Opencode flags.
    pub fn opencode_extras(&self) -> Option<&OpencodeExtras> {
        match &self.extras {
            Some(HarnessExtras::Opencode(e)) => Some(e),
            _ => None,
        }
    }

    /// Build a human-readable summary of active options for the ON confirmation message.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref m) = self.model {
            parts.push(format!("model={}", m));
        }
        // Claude extras
        if let Some(c) = self.claude_extras() {
            if let Some(ref e) = c.effort {
                parts.push(format!("effort={}", e));
            }
            if let Some(ref sp) = c.system_prompt {
                let preview: String = sp.chars().take(30).collect();
                let suffix = if sp.chars().count() > 30 { "..." } else { "" };
                parts.push(format!("system-prompt=\"{}{}\"", preview, suffix));
            }
            if let Some(ref asp) = c.append_system_prompt {
                let preview: String = asp.chars().take(30).collect();
                let suffix = if asp.chars().count() > 30 { "..." } else { "" };
                parts.push(format!("append-system-prompt=\"{}{}\"", preview, suffix));
            }
            if let Some(n) = c.max_turns {
                parts.push(format!("max-turns={}", n));
            }
            if let Some(ref s) = c.settings {
                parts.push(format!("settings={}", s));
            }
            if let Some(ref m) = c.mcp_config {
                parts.push(format!("mcp-config={}", m.display()));
            }
            if let Some(ref pm) = c.permission_mode {
                parts.push(format!("permission-mode={}", pm));
            }
        }
        for d in &self.add_dirs {
            parts.push(format!("add-dir={}", d.display()));
        }
        if let Some(ref s) = self.schema {
            parts.push(format!("schema={}", s));
        }
        if let Some(ref n) = self.name {
            parts.push(format!("name={}", n));
        }
        if let Some(ref r) = self.resume {
            parts.push(format!("resume={}", r));
        }
        // Opencode extras
        if let Some(o) = self.opencode_extras() {
            if let Some(ref a) = o.agent {
                parts.push(format!("agent={}", a));
            }
            if let Some(ref t) = o.title {
                parts.push(format!("title={}", t));
            }
            if o.share {
                parts.push("share".to_string());
            }
            if o.pure {
                parts.push("pure".to_string());
            }
            if o.fork {
                parts.push("fork".to_string());
            }
        }
        if self.continue_last {
            parts.push("continue-last".to_string());
        }
        // Gemini extras
        if let Some(g) = self.gemini_extras() {
            if let Some(ref am) = g.approval_mode {
                parts.push(format!("approval-mode={}", am));
            }
        }
        // Codex extras
        if let Some(c) = self.codex_extras() {
            if let Some(ref s) = c.sandbox {
                parts.push(format!("sandbox={}", s));
            }
            if let Some(ref p) = c.profile {
                parts.push(format!("profile={}", p));
            }
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
        /// Options parsed from flags before the prompt text (e.g. `--schema=foo`).
        options: HarnessOptions,
    },
    /// Capture and send the current terminal screen
    Screen,
    /// Enter interactive harness mode — plain text routes to the harness.
    /// If `initial_prompt` is set, the mode is enabled AND the prompt is
    /// sent as the first message in one go
    /// (e.g. `: claude on --resume review please look at the bag`).
    HarnessOn {
        harness: HarnessKind,
        options: HarnessOptions,
        initial_prompt: Option<String>,
    },
    /// Exit interactive harness mode — plain text routes back to tmux
    HarnessOff {
        harness: HarnessKind,
    },
    /// Raw CLI subcommand passthrough (e.g. `: opencode models openrouter`,
    /// `: gemini sessions`). Execution captures stdout and surfaces it to chat.
    HarnessSubcommand {
        harness: HarnessKind,
        subcommand: HarnessSubcommandKind,
        args: Vec<String>,
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

/// Replace em-dash (—, U+2014) with `--`.
/// Mobile keyboards (iOS autocorrect) frequently convert `--` to `—`, which
/// breaks flag parsing. Applying this normalization before tokenizing restores
/// the intended flag form.
///
/// En-dash (–, U+2013) is intentionally NOT normalized — it appears in
/// legitimate prose (e.g. date ranges) and is preserved per the convention
/// established by `tmux.rs::normalize_quotes`.
fn normalize_em_dash(s: &str) -> String {
    s.replace('—', "--") // U+2014 only
}

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
                // Normalize em-dash (U+2014 → --) once at the top of the harness
                // dispatch block, before any on/off/subcommand/prompt branching.
                // This ensures mobile users typing `—name foo` get `--name foo`
                // regardless of which path (HarnessOn, HarnessPrompt, subcommand)
                // they end up on.
                let after_harness_raw = rest[first_word.len()..].trim();
                let after_harness_owned = normalize_em_dash(after_harness_raw);
                let after_harness = after_harness_owned.as_str();
                if after_harness == "on" {
                    return Ok(ParsedCommand::HarnessOn {
                        harness: kind,
                        options: HarnessOptions::default(),
                        initial_prompt: None,
                    });
                }
                // `: claude on --model sonnet --add-dir ../lib`
                // `: claude on --resume review please look at the bag`
                //   (flags first, then a prompt — we enable the mode AND
                //   fire the prompt as the first message.)
                //
                // `: claude on <prompt>` (no flags) is rejected: `on` is
                // a mode toggle, not a one-shot — if the user wants a
                // quick prompt they should use `: claude <prompt>`. This
                // also prevents muscle-memory typos like `: claude on hi`
                // from silently entering interactive mode and burning
                // a Claude call on a stray word.
                if let Some(on_args) = after_harness.strip_prefix("on ") {
                    let trimmed = on_args.trim();
                    let (options, prompt) = split_prompt_options(trimmed, kind)?;
                    if options.is_empty() {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "`on` expects flags (e.g. `--name foo`, `--resume bar`). \
                             For a one-shot prompt use `: {} {}` instead.",
                            kind.name().to_lowercase(),
                            trimmed
                        )));
                    }
                    // `--fork` is rejected at parse time per-harness; no further
                    // post-parse check needed here.
                    let initial_prompt = if prompt.is_empty() {
                        None
                    } else {
                        Some(prompt)
                    };
                    return Ok(ParsedCommand::HarnessOn {
                        harness: kind,
                        options,
                        initial_prompt,
                    });
                }
                if after_harness == "off" {
                    return Ok(ParsedCommand::HarnessOff { harness: kind });
                }

                // Opencode-only: raw CLI subcommand passthrough (models, stats, sessions,
                // providers, export) and a destructive-command blocklist. Other harnesses
                // fall straight through to prompt parsing.
                // Em-dash normalization already applied above (after_harness is normalized).
                if kind == HarnessKind::Opencode && !after_harness.is_empty() {
                    let mut subword_iter = after_harness.split_whitespace().peekable();
                    let first = subword_iter.next().unwrap_or("");
                    let second = subword_iter.peek().copied().unwrap_or("");

                    // Match user-friendly 1-word aliases and native 2-word opencode forms.
                    let (sub, consumed_two) = match (first, second) {
                        // 1-word aliases (our user-friendly forms)
                        ("models", _) => (Some(OpencodeSubcommand::Models), false),
                        ("stats", _) => (Some(OpencodeSubcommand::Stats), false),
                        ("sessions", _) => (Some(OpencodeSubcommand::Sessions), false),
                        ("providers", _) => (Some(OpencodeSubcommand::Providers), false),
                        ("export", _) => (Some(OpencodeSubcommand::Export), false),
                        // 2-word native opencode forms — consume both words
                        ("session", "list") | ("session", "ls") => {
                            (Some(OpencodeSubcommand::Sessions), true)
                        }
                        ("auth", "list") | ("auth", "ls") => {
                            (Some(OpencodeSubcommand::Providers), true)
                        }
                        _ => (None, false),
                    };

                    if let Some(subcommand) = sub {
                        // Consume the second word if the native 2-word form was matched.
                        if consumed_two {
                            let _ = subword_iter.next();
                        }
                        let rest_args: Vec<String> = subword_iter.map(String::from).collect();

                        // `export` REQUIRES an argument; reject early with a helpful message.
                        if matches!(subcommand, OpencodeSubcommand::Export) && rest_args.is_empty()
                        {
                            return Err(ParseError::InvalidHarnessOption(
                                "`: opencode export` needs a sessionID (e.g. `: opencode export ses_01HABCDEF...`)".into()
                            ));
                        }
                        return Ok(ParsedCommand::HarnessSubcommand {
                            harness: kind,
                            subcommand: HarnessSubcommandKind::Opencode(subcommand),
                            args: rest_args,
                        });
                    }

                    // Blocklist: destructive / interactive / TTY-bound subcommands.
                    // Return a chat-safe error — DO NOT silently treat these as prompts.
                    // Note: bare `session` and `auth` (without a recognized sub-word) also
                    // fall into this blocklist.
                    const BLOCKED: &[&str] = &[
                        "uninstall",
                        "upgrade",
                        "auth",
                        "session",
                        "login",
                        "logout",
                        "serve",
                        "web",
                        "acp",
                        "attach",
                        "import",
                        "mcp",
                        "agent",
                        "github",
                        "debug",
                        "tui",
                    ];
                    if BLOCKED.contains(&first) {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "`opencode {}` is not available from chat — run it in your terminal. \
                             Safe chat subcommands: models, stats, sessions, providers, export.",
                            first
                        )));
                    }

                    // Fallthrough: not a subcommand, treat as prompt (existing behavior).
                }

                // Gemini chat-safe subcommands: route `sessions` and `extensions`
                // to the corresponding `--list-*` flags. Their interactive forms
                // (`extensions <add|remove|...>`, `mcp`, `skills`, `hooks`) and
                // anything else fall to the blocked list.
                if kind == HarnessKind::Gemini && !after_harness.is_empty() {
                    let mut subword_iter = after_harness.split_whitespace().peekable();
                    let first = subword_iter.next().unwrap_or("");
                    let second = subword_iter.peek().copied().unwrap_or("");

                    // Chat-safe routing — only the bare 1-word forms are
                    // accepted. `extensions install` / `extensions remove` /
                    // anything that would mutate state must keep falling
                    // through to the blocked-list rejection.
                    let routed = match (first, second) {
                        ("sessions", "") => Some(GeminiSubcommand::Sessions),
                        ("extensions", "") => Some(GeminiSubcommand::Extensions),
                        _ => None,
                    };

                    if let Some(subcommand) = routed {
                        let rest_args: Vec<String> = subword_iter.map(String::from).collect();
                        return Ok(ParsedCommand::HarnessSubcommand {
                            harness: kind,
                            subcommand: HarnessSubcommandKind::Gemini(subcommand),
                            args: rest_args,
                        });
                    }

                    // Block gemini's native interactive/destructive subcommands
                    // so `: gemini mcp`/`update`/etc. return a chat-safe error
                    // instead of silently hanging (mcp opens a TTY, update needs
                    // interactive confirmation, etc.). `extensions <sub>` falls
                    // here because the bare `extensions` form was caught above.
                    const GEMINI_BLOCKED: &[&str] =
                        &["update", "mcp", "extensions", "skills", "hooks"];
                    if GEMINI_BLOCKED.contains(&first) {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "`gemini {}` is not available from chat — run it in your terminal. \
                             Safe chat subcommands: sessions, extensions.",
                            first
                        )));
                    }
                }

                // Codex chat-safe subcommands (F6 + F7): route `sessions`,
                // `apply <task_id>`, and the `cloud {list,status,diff,apply,exec}`
                // subgroup. Each is single-shot — the "submit then apply" cloud
                // lifecycle is just two independent CLI calls from the user's
                // perspective, which fits terminus's short-lived-subprocess model.
                // Other native subcommands (`login`, `mcp`, `app`, etc.) still
                // return a chat-safe error. Named-session resume continues via
                // the `--resume <name>` flag, independent of codex's `resume`
                // subcommand.
                if kind == HarnessKind::Codex && !after_harness.is_empty() {
                    let mut subword_iter = after_harness.split_whitespace().peekable();
                    let first = subword_iter.next().unwrap_or("");
                    let second = subword_iter.peek().copied().unwrap_or("");

                    // Route the chat-safe forms first, before the blocked-list check.
                    // 1-word: `sessions`, `apply` (if followed by a task id arg).
                    // 2-word: `cloud {list|status|diff|apply|exec}`.
                    let (sub, consumed_two) = match (first, second) {
                        ("sessions", _) => (Some(CodexSubcommand::Sessions), false),
                        ("apply", _) => (Some(CodexSubcommand::Apply), false),
                        ("cloud", "list") => (
                            Some(CodexSubcommand::Cloud(CodexCloudSubcommand::List)),
                            true,
                        ),
                        ("cloud", "status") => (
                            Some(CodexSubcommand::Cloud(CodexCloudSubcommand::Status)),
                            true,
                        ),
                        ("cloud", "diff") => (
                            Some(CodexSubcommand::Cloud(CodexCloudSubcommand::Diff)),
                            true,
                        ),
                        ("cloud", "apply") => (
                            Some(CodexSubcommand::Cloud(CodexCloudSubcommand::Apply)),
                            true,
                        ),
                        ("cloud", "exec") => (
                            Some(CodexSubcommand::Cloud(CodexCloudSubcommand::Exec)),
                            true,
                        ),
                        _ => (None, false),
                    };

                    if let Some(subcommand) = sub {
                        if consumed_two {
                            let _ = subword_iter.next();
                        }
                        let rest_args: Vec<String> = subword_iter.map(String::from).collect();

                        // Subcommands that REQUIRE a task_id arg — produce both the
                        // boolean and the display label from a single exhaustive
                        // match so a future variant addition that needs a task_id
                        // can't slip past the rejection guard. (Architect P1.)
                        let task_label: Option<&str> = match &subcommand {
                            CodexSubcommand::Apply => Some("apply"),
                            CodexSubcommand::Cloud(CodexCloudSubcommand::Status) => {
                                Some("cloud status")
                            }
                            CodexSubcommand::Cloud(CodexCloudSubcommand::Diff) => {
                                Some("cloud diff")
                            }
                            CodexSubcommand::Cloud(CodexCloudSubcommand::Apply) => {
                                Some("cloud apply")
                            }
                            CodexSubcommand::Sessions
                            | CodexSubcommand::Cloud(CodexCloudSubcommand::List)
                            | CodexSubcommand::Cloud(CodexCloudSubcommand::Exec) => None,
                        };
                        if let Some(label) = task_label {
                            if rest_args.is_empty() {
                                return Err(ParseError::InvalidHarnessOption(format!(
                                    "`: codex {}` requires a task id (e.g. `: codex {} task_abc123`)",
                                    label, label
                                )));
                            }
                        }
                        // `cloud exec` requires `--env <env_id>` upstream; let codex
                        // surface its own usage error rather than gatekeeping here.
                        // `sessions` takes no arguments — reject extras up front so
                        // the user gets a chat-side message rather than a confusing
                        // codex error from `codex resume --all extra args`. (Critic P2.)
                        if matches!(subcommand, CodexSubcommand::Sessions) && !rest_args.is_empty()
                        {
                            return Err(ParseError::InvalidHarnessOption(
                                "`: codex sessions` takes no arguments — drop the extra tokens"
                                    .into(),
                            ));
                        }

                        return Ok(ParsedCommand::HarnessSubcommand {
                            harness: kind,
                            subcommand: HarnessSubcommandKind::Codex(subcommand),
                            args: rest_args,
                        });
                    }

                    const CODEX_BLOCKED: &[&str] = &[
                        // auth / TTY-bound
                        "login",
                        "logout",
                        // server / desktop modes
                        "mcp",
                        "mcp-server",
                        "app",
                        "app-server",
                        "exec-server",
                        // management / inspection
                        "plugin",
                        "completion",
                        "features",
                        "debug",
                        "sandbox", // the subcommand form, NOT --sandbox flag
                        // interactive-only or out of v1.1 scope
                        "review",
                        "resume", // bare subcommand; `: codex --resume <name>` still works
                        "fork",
                        // bare `cloud` (no sub) — must be a recognized 2-word form
                        "cloud",
                    ];
                    if CODEX_BLOCKED.contains(&first) {
                        // Tailor the chat-safe error message per category for clearer guidance.
                        let detail = match first {
                            "login" | "logout" => "codex auth must be run from your terminal",
                            "mcp" | "mcp-server" => {
                                "MCP management is not exposed to chat; run from terminal"
                            }
                            "app" | "app-server" | "exec-server" => {
                                "codex desktop/server modes are not exposed to chat"
                            }
                            "cloud" => {
                                "use one of: `cloud list`, `cloud status <id>`, `cloud diff <id>`, \
                                 `cloud apply <id>`, `cloud exec --env <env_id> <query>`"
                            }
                            "resume" => {
                                "use `: codex --resume <name>` for named-session resume; \
                                         the bare `resume` subcommand is not exposed to chat in v1"
                            }
                            "fork" => "session forking is not exposed to chat in v1",
                            _ => "not exposed to chat in v1; run from terminal",
                        };
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "`codex {}` is not available from chat — {}.",
                            first, detail
                        )));
                    }
                }

                if !after_harness.is_empty() {
                    // Extract any leading --flags before the actual prompt text.
                    // e.g. `: claude --schema=foo my prompt` → options={schema: "foo"}, prompt="my prompt"
                    // `--fork` mismatch and other cross-harness flag misuse is
                    // rejected at parse time inside `split_prompt_options`.
                    let (options, prompt) = split_prompt_options(after_harness, kind)?;
                    return Ok(ParsedCommand::HarnessPrompt {
                        harness: kind,
                        prompt,
                        options,
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

/// Split leading flag tokens from a prompt string.
///
/// Tokens starting with `--` (or `-` short flags) are consumed as options
/// until a non-flag token is encountered. The remainder is returned as the
/// prompt text. The `kind` parameter routes per-harness flags into the right
/// [`HarnessExtras`] variant and rejects cross-harness misuse (e.g.
/// `: claude --sandbox=foo` fails fast rather than silently dropping).
///
/// Example: `"--schema=foo my prompt"` → `(HarnessOptions { schema: Some("foo"), .. }, "my prompt")`
fn split_prompt_options(
    input: &str,
    kind: HarnessKind,
) -> std::result::Result<(HarnessOptions, String), ParseError> {
    let normalized = normalize_quotes(input);
    let tokens = shell_tokenize(&normalized);

    // Find the split point: first token that doesn't start with `-`.
    //
    // Boolean flags (`--share`, `--pure`, `--fork`) and the conditional
    // `--continue` (bare, no name) must NOT consume the next token as a value.
    // `--continue` is treated as boolean when the next token starts with `-`
    // or there is no next token.
    const BOOLEAN_FLAGS: &[&str] = &["--share", "--pure", "--fork"];

    let mut flag_end = 0;
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.starts_with('-') {
            // `--flag=value` form — single token, no look-ahead needed.
            if token.contains('=') {
                i += 1;
            } else if BOOLEAN_FLAGS.contains(&token.as_str()) {
                // Boolean flag — no value token.
                i += 1;
            } else if token == "--continue" {
                // Bare `--continue`: boolean if no next token or next is a flag.
                let next = tokens.get(i + 1).map(|t| t.as_str()).unwrap_or("");
                if next.is_empty() || next.starts_with('-') {
                    i += 1;
                } else {
                    i += 2; // `--continue <name>` form
                }
            } else {
                // `--flag value` form — skip flag and value.
                i += 2;
            }
            flag_end = i;
        } else {
            break;
        }
    }

    // The flag portion is everything up to flag_end.
    let flag_tokens = &tokens[..flag_end.min(tokens.len())];
    let prompt_tokens = &tokens[flag_end.min(tokens.len())..];
    let prompt = prompt_tokens.join(" ");

    // If there were no flags, just return with empty options.
    if flag_tokens.is_empty() {
        return Ok((HarnessOptions::default(), prompt));
    }

    // Parse directly from tokens — re-joining with spaces would lose the
    // quote context on multi-word values (e.g. `--system-prompt "a b c"`
    // would become `--system-prompt a b c`).
    let options = parse_harness_options_tokens(flag_tokens, kind)?;

    Ok((options, prompt))
}

/// Validate a session name used in harness `--name` / `--resume` flags.
/// Same rules as `validate_session_name` (alphanumeric, hyphens, underscores,
/// max 64 chars) but returns `ParseError::InvalidHarnessOption` with the
/// originating flag name for context.
fn validate_harness_session_name(name: &str, flag: &str) -> std::result::Result<(), ParseError> {
    if name.is_empty() {
        return Err(ParseError::InvalidHarnessOption(format!(
            "{} requires a non-empty session name",
            flag
        )));
    }
    if name.chars().count() > MAX_SESSION_NAME_LEN {
        return Err(ParseError::InvalidHarnessOption(format!(
            "Session name too long (max {} characters)",
            MAX_SESSION_NAME_LEN
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ParseError::InvalidHarnessOption(
            "Session name can only contain letters, numbers, hyphens, and underscores".to_string(),
        ));
    }
    Ok(())
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
/// Parse harness options from a pre-tokenized slice. Callers are expected
/// to have already run `shell_tokenize` on a `normalize_quotes`-cleaned
/// input (typically via `split_prompt_options`) so that quoted multi-word
/// values like `--system-prompt "You are a Rust expert"` survive intact.
fn parse_harness_options_tokens(
    tokens: &[String],
    kind: HarnessKind,
) -> std::result::Result<HarnessOptions, ParseError> {
    let mut opts = HarnessOptions::default();
    // Pre-allocate the right extras variant so we always have a place to
    // route per-harness flags. Trimmed back to `None` at the end if nothing
    // was set, so `is_empty()` and the dispatcher don't see a noisy default.
    let mut claude_e = ClaudeExtras::default();
    let mut codex_e = CodexExtras::default();
    let mut gemini_e = GeminiExtras::default();
    let mut opencode_e = OpencodeExtras::default();

    fn reject_for(kind: HarnessKind, flag: &str, allowed: &str) -> ParseError {
        ParseError::InvalidHarnessOption(format!(
            "{} is only supported by {} (you used `: {}`)",
            flag,
            allowed,
            kind.name().to_lowercase()
        ))
    }

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
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--effort", "claude"));
                }
                let val = get_value!();
                match val.to_lowercase().as_str() {
                    "low" | "medium" | "high" | "max" => {
                        claude_e.effort = Some(val.to_lowercase());
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
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--system-prompt", "claude"));
                }
                let val = get_value!();
                claude_e.system_prompt = Some(val);
            }
            "--append-system-prompt" => {
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--append-system-prompt", "claude"));
                }
                let val = get_value!();
                claude_e.append_system_prompt = Some(val);
            }
            "--add-dir" | "-d" => {
                // Shared between claude and codex; opencode/gemini ignore unknown
                // CLI flags but rejecting here gives clearer feedback.
                if !matches!(kind, HarnessKind::Claude | HarnessKind::Codex) {
                    return Err(reject_for(kind, "--add-dir", "claude and codex"));
                }
                let val = get_value!();
                opts.add_dirs.push(PathBuf::from(val));
            }
            "--max-turns" | "-t" => {
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--max-turns", "claude"));
                }
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
                claude_e.max_turns = Some(n);
            }
            "--settings" => {
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--settings", "claude"));
                }
                let val = get_value!();
                claude_e.settings = Some(val);
            }
            "--mcp-config" => {
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--mcp-config", "claude"));
                }
                let val = get_value!();
                claude_e.mcp_config = Some(PathBuf::from(val));
            }
            "--permission-mode" | "-p" => {
                if kind != HarnessKind::Claude {
                    return Err(reject_for(kind, "--permission-mode", "claude"));
                }
                let val = get_value!();
                match val.as_str() {
                    "default" | "acceptEdits" | "plan" | "bypassPermissions" => {
                        claude_e.permission_mode = Some(val);
                    }
                    _ => {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "Invalid permission mode '{}' — expected default, acceptEdits, plan, or bypassPermissions",
                            val
                        )));
                    }
                }
            }
            "--schema" => {
                let val = get_value!();
                opts.schema = Some(val);
            }
            "--name" | "-n" => {
                let val = get_value!();
                validate_harness_session_name(&val, flag)?;
                opts.name = Some(val);
            }
            "--resume" => {
                let val = get_value!();
                validate_harness_session_name(&val, flag)?;
                opts.resume = Some(val);
            }
            "--continue" => {
                // Peek at the next token: if it starts with `--` or there is no
                // next token, treat as bare `--continue` (continue last session).
                // Otherwise behave as an alias for `--resume <name>`.
                let next_is_flag_or_missing = match eq_value {
                    Some(_) => false, // `--continue=name` form always takes the value
                    None => {
                        let peek = tokens.get(i + 1).map(|t| t.as_str()).unwrap_or("");
                        peek.is_empty() || peek.starts_with('-')
                    }
                };
                if next_is_flag_or_missing && eq_value.is_none() {
                    opts.continue_last = true;
                } else {
                    let val = get_value!();
                    validate_harness_session_name(&val, flag)?;
                    opts.resume = Some(val);
                }
            }
            "--title" => {
                if kind != HarnessKind::Opencode {
                    return Err(reject_for(kind, "--title", "opencode"));
                }
                let val = get_value!();
                opencode_e.title = Some(val);
            }
            "--agent" => {
                if kind != HarnessKind::Opencode {
                    return Err(reject_for(kind, "--agent", "opencode"));
                }
                let val = get_value!();
                opencode_e.agent = Some(val);
            }
            "--share" => {
                if kind != HarnessKind::Opencode {
                    return Err(reject_for(kind, "--share", "opencode"));
                }
                opencode_e.share = true;
            }
            "--pure" => {
                if kind != HarnessKind::Opencode {
                    return Err(reject_for(kind, "--pure", "opencode"));
                }
                opencode_e.pure = true;
            }
            "--fork" => {
                // Reject at parse time for non-opencode harnesses with the
                // existing harness-specific messages (preserves user-facing
                // wording from the pre-F10 call-site check).
                match kind {
                    HarnessKind::Opencode => {
                        opencode_e.fork = true;
                    }
                    HarnessKind::Gemini => {
                        return Err(ParseError::InvalidHarnessOption(
                            "gemini does not support --fork — remove the flag".into(),
                        ));
                    }
                    HarnessKind::Codex => {
                        return Err(ParseError::InvalidHarnessOption(
                            "codex does not support --fork in non-interactive mode \
                             (use `codex fork` from your terminal). Remove the flag."
                                .into(),
                        ));
                    }
                    HarnessKind::Claude => {
                        return Err(reject_for(kind, "--fork", "opencode"));
                    }
                }
            }
            "--approval-mode" => {
                if kind != HarnessKind::Gemini {
                    return Err(reject_for(kind, "--approval-mode", "gemini"));
                }
                let val = get_value!();
                // Gemini accepts: default | auto_edit | yolo | plan. The
                // `on-request` value is rejected outright because it would
                // deadlock the harness with no TTY for prompts. (Codex's
                // own approval system is unrelated and lives behind
                // `-s <sandbox>` / permission profiles in 0.128+.)
                match val.as_str() {
                    "default" | "auto_edit" | "yolo" | "plan" => {
                        gemini_e.approval_mode = Some(val);
                    }
                    "on-request" => {
                        return Err(ParseError::InvalidHarnessOption(
                            "--approval-mode on-request would deadlock the harness \
                             (no TTY for approval prompts) — pick default, auto_edit, \
                             yolo, or plan instead"
                                .into(),
                        ));
                    }
                    _ => {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "Invalid --approval-mode '{}' — expected default, auto_edit, yolo, or plan",
                            val
                        )));
                    }
                }
            }
            "--sandbox" => {
                if kind != HarnessKind::Codex {
                    return Err(reject_for(kind, "--sandbox", "codex"));
                }
                let val = get_value!();
                match val.as_str() {
                    "read-only" | "workspace-write" | "danger-full-access" => {
                        codex_e.sandbox = Some(val);
                    }
                    _ => {
                        return Err(ParseError::InvalidHarnessOption(format!(
                            "Invalid --sandbox '{}' — expected read-only, workspace-write, or danger-full-access",
                            val
                        )));
                    }
                }
            }
            "--profile" => {
                if kind != HarnessKind::Codex {
                    return Err(reject_for(kind, "--profile", "codex"));
                }
                // No `-p` short alias: `-p` is already taken by --permission-mode
                // for the Claude harness. Codex users use the long form.
                let val = get_value!();
                codex_e.profile = Some(val);
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
                    "Unknown option '{}'. Supported: --model, --effort, --system-prompt, --append-system-prompt, --add-dir, --max-turns, --settings, --mcp-config, --permission-mode, --schema, --name, --resume, --continue, --agent, --title, --share, --pure, --fork, --approval-mode, --sandbox, --profile",
                    other
                )));
            }
        }
        i += 1;
    }

    // Reject mutually exclusive options
    if claude_e.system_prompt.is_some() && claude_e.append_system_prompt.is_some() {
        return Err(ParseError::InvalidHarnessOption(
            "Cannot use both --system-prompt and --append-system-prompt".to_string(),
        ));
    }
    if opts.name.is_some() && opts.resume.is_some() {
        return Err(ParseError::InvalidHarnessOption(
            "Cannot use both --name and --resume/--continue".to_string(),
        ));
    }
    if opts.continue_last && (opts.resume.is_some() || opts.name.is_some()) {
        return Err(ParseError::InvalidHarnessOption(
            "Cannot use bare --continue with --name or --resume".to_string(),
        ));
    }
    if opencode_e.fork && !opts.continue_last && opts.resume.is_none() {
        return Err(ParseError::InvalidHarnessOption(
            "--fork requires --continue or --resume to be set".to_string(),
        ));
    }

    // Materialize the right extras variant. Use the per-struct `is_empty()`
    // (not `== Default::default()`) so a future field whose Default value is
    // meaningful — e.g. an integer defaulting to `0` being a real choice —
    // doesn't silently misfire this guard. (Code-reviewer LOW-1.)
    let extras = match kind {
        HarnessKind::Claude => {
            if claude_e.is_empty() {
                None
            } else {
                Some(HarnessExtras::Claude(claude_e))
            }
        }
        HarnessKind::Codex => {
            if codex_e.is_empty() {
                None
            } else {
                Some(HarnessExtras::Codex(codex_e))
            }
        }
        HarnessKind::Gemini => {
            if gemini_e.is_empty() {
                None
            } else {
                Some(HarnessExtras::Gemini(gemini_e))
            }
        }
        HarnessKind::Opencode => {
            if opencode_e.is_empty() {
                None
            } else {
                Some(HarnessExtras::Opencode(opencode_e))
            }
        }
    };
    opts.extras = extras;

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
    /// Also splits on shell chaining operators (`;`, `|`, `&&`, `||`) and checks
    /// each segment, preventing blocklist bypass via chained commands such as
    /// `echo hello ; sudo reboot` or `echo $(sudo reboot)`.
    #[must_use = "ignoring a blocklist check is a security risk"]
    pub fn is_blocked(&self, command: &str) -> bool {
        if self.is_blocked_single(command) {
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
                if self.is_blocked_single(line) {
                    return true;
                }
            }
        }
        false
    }

    /// Check a single line/segment for blocklist matches.
    /// Splits on shell chaining operators and checks each resulting segment,
    /// then also checks the full string as a whole.
    fn is_blocked_single(&self, command: &str) -> bool {
        let normalized = Self::normalize_command(command);
        // Check the full command first
        if self
            .patterns
            .iter()
            .any(|p| p.is_match(command) || p.is_match(&normalized))
        {
            return true;
        }

        // Split on shell chaining operators and check each segment.
        // This prevents bypasses like `echo hello ; sudo reboot` or
        // `ls | sudo tee /etc/passwd` from evading a `sudo` pattern.
        let segments: Vec<&str> = command
            .split(&[';', '|'][..])
            .flat_map(|s| s.split("&&"))
            .flat_map(|s| s.split("||"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        // Only bother if splitting produced more than one segment
        if segments.len() > 1 {
            for seg in &segments {
                let norm_seg = Self::normalize_command(seg);
                if self
                    .patterns
                    .iter()
                    .any(|p| p.is_match(seg) || p.is_match(&norm_seg))
                {
                    return true;
                }
            }
        }

        // Extract content from $(...) and backtick subcommand substitutions
        // e.g. `echo $(sudo reboot)` or `echo \`sudo reboot\`` → check `sudo reboot`
        for (open, close) in &[("$(", ')'), ("`", '`')] {
            if command.contains(open) {
                let mut rest = command;
                while let Some(start) = rest.find(open) {
                    rest = &rest[start + open.len()..];
                    if let Some(end) = rest.find(*close) {
                        let inner = rest[..end].trim();
                        if !inner.is_empty() {
                            let norm_inner = Self::normalize_command(inner);
                            if self
                                .patterns
                                .iter()
                                .any(|p| p.is_match(inner) || p.is_match(&norm_inner))
                            {
                                return true;
                            }
                        }
                        rest = &rest[end + 1..];
                    } else {
                        break;
                    }
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
                prompt: "explain this code".into(),
                options: HarnessOptions::default(),
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
                initial_prompt: None,
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
                initial_prompt: None,
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
                prompt: "explain this".into(),
                options: HarnessOptions::default(),
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
                prompt: "explain this".into(),
                options: HarnessOptions::default(),
            }
        );
    }

    #[test]
    fn parse_multiline_harness_prompt_allowed() {
        assert_eq!(
            ParsedCommand::parse(": claude explain\nthis code", ':').unwrap(),
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Claude,
                prompt: "explain\nthis code".into(),
                options: HarnessOptions::default(),
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
                initial_prompt: None,
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
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        effort: Some("high".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                initial_prompt: None,
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
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_claude_on_with_max_turns() {
        let cmd = ParsedCommand::parse(": claude on -t 5", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        max_turns: Some(5),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        system_prompt: Some("You are a Rust expert".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_claude_on_with_multiple_options() {
        let cmd = ParsedCommand::parse(
            ": claude on --model sonnet --effort high --add-dir ../lib -t 10",
            ':',
        )
        .unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    model: Some("sonnet".into()),
                    add_dirs: vec![PathBuf::from("../lib")],
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        effort: Some("high".into()),
                        max_turns: Some(10),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        settings: Some("./settings.json".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        mcp_config: Some(PathBuf::from("./mcp.json")),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        append_system_prompt: Some("Always use TypeScript".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_claude_on_with_permission_mode() {
        let cmd = ParsedCommand::parse(": claude on --permission-mode acceptEdits", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        permission_mode: Some("acceptEdits".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_claude_on_with_short_permission_mode_flag() {
        let cmd = ParsedCommand::parse(": claude on -p bypassPermissions", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        permission_mode: Some("bypassPermissions".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_claude_on_invalid_permission_mode_rejected() {
        let err = ParsedCommand::parse(": claude on --permission-mode yolo", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Invalid permission mode"));
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
            extras: Some(HarnessExtras::Claude(ClaudeExtras {
                effort: Some("high".into()),
                ..Default::default()
            })),
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

    #[test]
    fn harness_options_is_empty_iff_summary_is_empty() {
        // Roundtrip invariant: a "no flags set" object must produce an empty
        // summary, and vice versa. Catches drift when a future field is added
        // to summary() without updating is_empty(), or vice versa.
        // (Test-engineer P1.)
        let empty = HarnessOptions::default();
        assert!(empty.is_empty(), "default must be is_empty");
        assert!(
            empty.summary().is_empty(),
            "default must have empty summary"
        );

        let with_model = HarnessOptions {
            model: Some("sonnet".into()),
            ..Default::default()
        };
        assert!(!with_model.is_empty(), "model-set must not be is_empty");
        assert!(
            !with_model.summary().is_empty(),
            "model-set must produce non-empty summary"
        );

        let with_codex_extras = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("read-only".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(
            !with_codex_extras.is_empty(),
            "codex-extras-set must not be is_empty"
        );
        assert!(
            !with_codex_extras.summary().is_empty(),
            "codex-extras-set must produce non-empty summary"
        );
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
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        effort: Some("high".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        system_prompt: Some("Be concise".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
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
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        system_prompt: Some("Be concise".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn harness_options_summary_truncates_long_system_prompt() {
        let long_prompt = "a".repeat(50);
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Claude(ClaudeExtras {
                system_prompt: Some(long_prompt),
                ..Default::default()
            })),
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

    // ── --schema flag tests ─────────────────────────────────────────────────

    #[test]
    fn schema_flag_parses() {
        // `: claude --schema=todos extract all TODOs` — one-shot with schema
        let cmd = ParsedCommand::parse(": claude --schema=todos extract all TODOs", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                prompt,
                options,
            } => {
                assert_eq!(harness, HarnessKind::Claude);
                assert_eq!(prompt, "extract all TODOs");
                assert_eq!(options.schema, Some("todos".into()));
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn schema_flag_space_separated_parses() {
        let cmd = ParsedCommand::parse(": claude --schema todos my prompt", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.schema, Some("todos".into()));
                assert_eq!(prompt, "my prompt");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn schema_flag_in_harness_on_parses() {
        let cmd = ParsedCommand::parse(": claude on --schema=todos", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn { options, .. } => {
                assert_eq!(options.schema, Some("todos".into()));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn schema_flag_with_other_flags_parses() {
        let cmd =
            ParsedCommand::parse(": claude --schema=todos --model sonnet my prompt", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.schema, Some("todos".into()));
                assert_eq!(options.model, Some("sonnet".into()));
                assert_eq!(prompt, "my prompt");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn schema_flag_rejected_if_unknown_flag_mixed() {
        // Unknown flags still rejected with helpful error.
        let err =
            ParsedCommand::parse(": claude on --schema=todos --unknown-flag x", ':').unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidHarnessOption(_)),
            "Expected InvalidHarnessOption, got: {:?}",
            err
        );
    }

    // ── Blocklist: shell chaining operator bypass prevention ────────────────

    #[test]
    fn blocklist_shell_chain_semicolon() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        // Semicolon-chained command — the `sudo reboot` segment must be caught
        assert!(bl.is_blocked("echo hello ; sudo reboot"));
        assert!(bl.is_blocked("ls -la;sudo reboot"));
    }

    #[test]
    fn blocklist_shell_chain_pipe() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("cat /etc/passwd | sudo tee /dev/null"));
    }

    #[test]
    fn blocklist_shell_chain_and_and() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("echo hello && sudo reboot"));
    }

    #[test]
    fn blocklist_shell_chain_or_or() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        assert!(bl.is_blocked("false || sudo reboot"));
    }

    #[test]
    fn blocklist_subcommand_substitution() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        // $(...) subcommand substitution — inner `sudo reboot` must be caught
        assert!(bl.is_blocked("echo $(sudo reboot)"));
        // backtick substitution — equivalent to $()
        assert!(bl.is_blocked("echo `sudo reboot`"));
    }

    #[test]
    fn blocklist_chain_safe_commands_not_blocked() {
        let bl = CommandBlocklist::from_config(&[r"sudo\s+".into()]).unwrap();
        // Chained safe commands must not be blocked
        assert!(!bl.is_blocked("echo hello ; ls -la"));
        assert!(!bl.is_blocked("echo hello && ls -la"));
    }

    // ── Named session flag tests ───────────────────────────────────────────

    #[test]
    fn parse_name_flag_long() {
        let cmd = ParsedCommand::parse(": claude --name auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.name, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_name_flag_short() {
        let cmd = ParsedCommand::parse(": claude -n auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.name, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_name_flag_equals() {
        let cmd = ParsedCommand::parse(": claude --name=auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.name, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_resume_flag() {
        let cmd = ParsedCommand::parse(": claude --resume auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.resume, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_continue_flag() {
        let cmd = ParsedCommand::parse(": claude --continue auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.resume, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_name_on_mode() {
        let cmd = ParsedCommand::parse(": claude on --name auth", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn { options, .. } => {
                assert_eq!(options.name, Some("auth".into()));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_resume_on_mode() {
        let cmd = ParsedCommand::parse(": claude on --resume auth", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn { options, .. } => {
                assert_eq!(options.resume, Some("auth".into()));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_em_dashed_name_on_mode() {
        // iOS autocorrect turns `--` into `—`; the normalizer should
        // fold it back so long flags keep working from mobile chat.
        let cmd = ParsedCommand::parse(": claude on \u{2014}name auth", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn { options, .. } => {
                assert_eq!(options.name, Some("auth".into()));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_em_dashed_resume_one_shot() {
        let cmd = ParsedCommand::parse(": claude \u{2014}resume auth fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.resume, Some("auth".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_em_dashed_name_one_shot_harness_prompt() {
        // Fix 1.2: em-dash normalization must apply on the HarnessPrompt path too.
        // `: claude —name foo hi` should parse to HarnessPrompt with name=Some("foo"),
        // prompt="hi" (not treat —name as prompt text).
        let cmd = ParsedCommand::parse(": claude \u{2014}name foo hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Claude);
                assert_eq!(options.name, Some("foo".into()));
                assert_eq!(prompt, "hi");
            }
            other => panic!("Expected HarnessPrompt with name=foo, got {:?}", other),
        }
    }

    #[test]
    fn en_dash_in_opencode_subcommand_is_not_normalized() {
        // Fix 1.7: en-dash (U+2013) is intentionally preserved per the convention
        // in tmux.rs::normalize_quotes. `: opencode stats –days 1` with an en-dash
        // should pass `–days` through as an arg unchanged (not `--days`).
        let cmd = ParsedCommand::parse(": opencode stats \u{2013}days 1", ':').unwrap();
        // `stats` is recognized as a subcommand keyword; `–days` (en-dash) is passed
        // through as an arg, NOT normalized to `--days`.
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Stats),
                args: vec!["\u{2013}days".into(), "1".into()],
            }
        );
    }

    // ── HarnessOn with trailing initial prompt ───────────────────────────────

    #[test]
    fn parse_on_with_resume_and_initial_prompt() {
        let cmd = ParsedCommand::parse(": claude on --resume review please look at the bag", ':')
            .unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                options,
                initial_prompt,
                ..
            } => {
                assert_eq!(options.resume, Some("review".into()));
                assert_eq!(initial_prompt.as_deref(), Some("please look at the bag"));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_on_with_em_dashed_resume_and_initial_prompt() {
        // The full mobile-chat case that triggered the bug report:
        // iOS autocorrect turns `--resume` into `—resume`, and `on` mode
        // must accept a trailing prompt.
        let cmd = ParsedCommand::parse(
            ": claude on \u{2014}resume review please review the background color of the bag",
            ':',
        )
        .unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                options,
                initial_prompt,
                ..
            } => {
                assert_eq!(options.resume, Some("review".into()));
                assert_eq!(
                    initial_prompt.as_deref(),
                    Some("please review the background color of the bag")
                );
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_on_with_continue_alias_no_prompt() {
        let cmd = ParsedCommand::parse(": claude on --continue review", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                options,
                initial_prompt,
                ..
            } => {
                assert_eq!(options.resume, Some("review".into()));
                assert!(initial_prompt.is_none());
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_on_with_name_and_initial_prompt() {
        let cmd = ParsedCommand::parse(": claude on --name auth fix the login bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                options,
                initial_prompt,
                ..
            } => {
                assert_eq!(options.name, Some("auth".into()));
                assert_eq!(initial_prompt.as_deref(), Some("fix the login bug"));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_on_with_quoted_system_prompt_then_initial_prompt() {
        // G1: quoted multi-word flag value followed by a prompt. Previously
        // untested — exercises the token round-trip through
        // `split_prompt_options` → `parse_harness_options_tokens`.
        let cmd = ParsedCommand::parse(
            ": claude on --system-prompt \"You are a Rust expert\" review this",
            ':',
        )
        .unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                options,
                initial_prompt,
                ..
            } => {
                assert_eq!(
                    options
                        .claude_extras()
                        .expect("claude extras")
                        .system_prompt
                        .as_deref(),
                    Some("You are a Rust expert")
                );
                assert_eq!(initial_prompt.as_deref(), Some("review this"));
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_on_name_and_resume_rejected() {
        // G2: mutex validation fires for `on` mode as well as one-shot.
        let err = ParsedCommand::parse(": claude on --name auth --resume auth", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Cannot use both"));
    }

    #[test]
    fn parse_on_without_flags_is_rejected() {
        // `: claude on <prompt>` without flags is disallowed: `on` is a
        // mode toggle, not a one-shot, and silently entering interactive
        // mode on a muscle-memory typo like `: claude on hi` would burn
        // a Claude call on a stray word. The error tells the user how to
        // do what they probably meant.
        let err = ParsedCommand::parse(": claude on hello there", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("on` expects flags"),
            "error should explain `on` needs flags: {}",
            msg
        );
        assert!(
            msg.contains(": claude hello there"),
            "error should suggest the one-shot form: {}",
            msg
        );
    }

    #[test]
    fn parse_on_without_flags_opencode_names_opencode() {
        // `: opencode on hello` must suggest `: opencode hello`, not `: claude hello`.
        let err = ParsedCommand::parse(": opencode on hello", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(
            msg.contains(": opencode hello"),
            "error should suggest `: opencode hello`, got: {}",
            msg
        );
    }

    #[test]
    fn parse_on_without_flags_gemini_names_gemini() {
        // `: gemini on hello` must suggest `: gemini hello`, not `: claude hello`.
        let err = ParsedCommand::parse(": gemini on hello", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(
            msg.contains(": gemini hello"),
            "error should suggest `: gemini hello`, got: {}",
            msg
        );
    }

    #[test]
    fn parse_name_and_resume_rejected() {
        let err = ParsedCommand::parse(": claude --name auth --resume auth fix", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Cannot use both"));
    }

    #[test]
    fn parse_name_invalid_session_name() {
        let err = ParsedCommand::parse(": claude --name foo:bar fix", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
    }

    #[test]
    fn parse_max_turns_short_flag_changed() {
        let cmd = ParsedCommand::parse(": claude on -t 5", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessOn {
                harness: HarnessKind::Claude,
                options: HarnessOptions {
                    extras: Some(HarnessExtras::Claude(ClaudeExtras {
                        max_turns: Some(5),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                initial_prompt: None,
            }
        );
    }

    #[test]
    fn parse_name_short_equals_rejected() {
        let err = ParsedCommand::parse(": claude -n=auth fix", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("long flags"));
    }

    #[test]
    fn parse_name_missing_value() {
        let err = ParsedCommand::parse(": claude --name", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn parse_resume_missing_value() {
        let err = ParsedCommand::parse(": claude --resume", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn parse_name_with_other_options() {
        let cmd = ParsedCommand::parse(": claude --name auth --model sonnet fix bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.name, Some("auth".into()));
                assert_eq!(options.model, Some("sonnet".into()));
                assert_eq!(prompt, "fix bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn harness_options_summary_includes_name() {
        let opts = HarnessOptions {
            name: Some("auth".into()),
            ..Default::default()
        };
        assert_eq!(opts.summary(), "name=auth");
    }

    #[test]
    fn harness_options_is_empty_false_when_name_set() {
        let opts = HarnessOptions {
            name: Some("auth".into()),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    // ── opencode parser tests ────────────────────────────────────────────────

    #[test]
    fn parse_opencode_bare_prompt() {
        let cmd = ParsedCommand::parse(": opencode hi there", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Opencode,
                prompt: "hi there".into(),
                options: HarnessOptions::default(),
            }
        );
    }

    #[test]
    fn parse_opencode_name_and_prompt() {
        let cmd = ParsedCommand::parse(": opencode --name auth fix the login bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.name, Some("auth".into()));
                assert!(options.resume.is_none());
                assert_eq!(prompt, "fix the login bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_resume_and_prompt() {
        let cmd = ParsedCommand::parse(": opencode --resume auth status?", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.resume, Some("auth".into()));
                assert!(options.name.is_none());
                assert_eq!(prompt, "status?");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_name_without_prompt() {
        let cmd = ParsedCommand::parse(": opencode on --name auth", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                harness,
                options,
                initial_prompt,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.name, Some("auth".into()));
                assert!(initial_prompt.is_none());
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_on_toggle() {
        let cmd = ParsedCommand::parse(": opencode on --name session1", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn { harness, .. } => {
                assert_eq!(harness, HarnessKind::Opencode);
            }
            other => panic!("Expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_on_em_dashed_name() {
        // iOS keyboard autocorrects `--` to `—` (U+2014). Should still parse as --name.
        let parsed = ParsedCommand::parse(": opencode on \u{2014}name review hi", ':').unwrap();
        match parsed {
            ParsedCommand::HarnessOn {
                harness,
                options,
                initial_prompt,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.name.as_deref(), Some("review"));
                assert_eq!(initial_prompt.as_deref(), Some("hi"));
            }
            other => panic!("expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_mutual_exclusion_rejected() {
        let err = ParsedCommand::parse(": opencode --name foo --resume bar hi", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Cannot use both"));
    }

    // ── HarnessSubcommand + blocklist tests ──────────────────────────────────

    #[test]
    fn parse_opencode_subcommand_models() {
        let cmd = ParsedCommand::parse(": opencode models", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Models),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_models_with_provider() {
        let cmd = ParsedCommand::parse(": opencode models openrouter", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Models),
                args: vec!["openrouter".into()],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_stats() {
        let cmd = ParsedCommand::parse(": opencode stats --days 7", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Stats),
                args: vec!["--days".into(), "7".into()],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_sessions() {
        let cmd = ParsedCommand::parse(": opencode sessions", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Sessions),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_providers() {
        let cmd = ParsedCommand::parse(": opencode providers", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Providers),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_export() {
        let cmd = ParsedCommand::parse(": opencode export ses_01HABCDEF", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Export),
                args: vec!["ses_01HABCDEF".into()],
            }
        );
    }

    #[test]
    fn parse_opencode_subcommand_export_without_id_rejected() {
        let err = ParsedCommand::parse(": opencode export", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("sessionID"));
    }

    #[test]
    fn parse_opencode_blocked_subcommand_bare_auth() {
        // Bare `auth` (no sub-word) is blocked.
        let err = ParsedCommand::parse(": opencode auth", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(msg.contains("not available from chat"), "got: {}", msg);
    }

    #[test]
    fn parse_opencode_blocked_subcommand_auth_login() {
        // `auth login` is NOT a recognized form (only `auth list`/`auth ls` are).
        // After failing to match the 2-word native form, `auth` hits the blocklist.
        let err = ParsedCommand::parse(": opencode auth login", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(msg.contains("not available from chat"), "got: {}", msg);
    }

    #[test]
    fn parse_opencode_blocked_subcommand_tui() {
        let err = ParsedCommand::parse(": opencode tui", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("not available from chat"));
    }

    #[test]
    fn parse_opencode_blocked_subcommand_upgrade() {
        let err = ParsedCommand::parse(": opencode upgrade", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
    }

    #[test]
    fn parse_opencode_blocked_subcommands_all_rejected() {
        let blocked = [
            "uninstall",
            "upgrade",
            "login",
            "logout",
            "serve",
            "web",
            "acp",
            "attach",
            "import",
            "mcp",
            "agent",
            "github",
            "debug",
            "tui",
            "session",
            "auth",
        ];
        for kw in &blocked {
            let input = format!(": opencode {}", kw);
            match ParsedCommand::parse(&input, ':') {
                Err(ParseError::InvalidHarnessOption(msg)) => {
                    assert!(
                        msg.contains("not available from chat")
                            || msg.contains("run it in your terminal")
                            || msg.contains("Safe chat subcommands"),
                        "blocked keyword `{}` did not surface a terminal-only error; got: {}",
                        kw,
                        msg
                    );
                }
                other => panic!(
                    "blocked keyword `{}` should be rejected, got {:?}",
                    kw, other
                ),
            }
        }
    }

    // Verify other harnesses don't get the subcommand path (fall through to prompt).
    #[test]
    fn parse_claude_models_is_prompt_not_subcommand() {
        let cmd = ParsedCommand::parse(": claude models openrouter", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessPrompt {
                harness: HarnessKind::Claude,
                prompt: "models openrouter".into(),
                options: HarnessOptions::default(),
            }
        );
    }

    // ── Gemini blocked-subcommand tests ─────────────────────────────────────

    #[test]
    fn parse_gemini_blocked_subcommands_all_rejected() {
        // After F5: bare `extensions` is chat-safe (routes to --list-extensions).
        // Blocked: `update`, `mcp`, `skills`, `hooks`, and `extensions <sub>`
        // (any sub-form of extensions; the bare form is the only safe one).
        let blocked = ["update", "mcp", "skills", "hooks"];
        for kw in &blocked {
            let input = format!(": gemini {}", kw);
            match ParsedCommand::parse(&input, ':') {
                Err(ParseError::InvalidHarnessOption(msg)) => {
                    assert!(
                        msg.contains("not available from chat")
                            && msg.contains(kw),
                        "blocked gemini keyword `{}` did not surface a terminal-only error; got: {}",
                        kw,
                        msg
                    );
                }
                other => panic!(
                    "blocked gemini keyword `{}` should be rejected, got {:?}",
                    kw, other
                ),
            }
        }
    }

    #[test]
    fn parse_gemini_extensions_subform_is_rejected() {
        // `: gemini extensions install foo` must NOT be routed (would hit the
        // interactive `gemini extensions install` subcommand). Only the bare
        // `extensions` form maps to `--list-extensions`.
        let input = ": gemini extensions install foo";
        match ParsedCommand::parse(input, ':') {
            Err(ParseError::InvalidHarnessOption(msg)) => {
                assert!(
                    msg.contains("not available from chat") && msg.contains("extensions"),
                    "extensions sub-form should be rejected; got: {}",
                    msg
                );
            }
            other => panic!("expected rejection, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_sessions_routes_to_list_sessions() {
        let cmd = ParsedCommand::parse(": gemini sessions", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                args,
            } => {
                assert_eq!(harness, HarnessKind::Gemini);
                assert!(matches!(
                    subcommand,
                    HarnessSubcommandKind::Gemini(GeminiSubcommand::Sessions)
                ));
                assert!(args.is_empty());
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_extensions_routes_to_list_extensions() {
        let cmd = ParsedCommand::parse(": gemini extensions", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                ..
            } => {
                assert_eq!(harness, HarnessKind::Gemini);
                assert!(matches!(
                    subcommand,
                    HarnessSubcommandKind::Gemini(GeminiSubcommand::Extensions)
                ));
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    // ── Gemini em-dash normalization ────────────────────────────────────────

    #[test]
    fn parse_gemini_em_dashed_name_is_normalized() {
        let cmd = ParsedCommand::parse(": gemini —name auth fix a bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Gemini);
                assert_eq!(options.name.as_deref(), Some("auth"));
                assert_eq!(prompt, "fix a bug");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    // ── --approval-mode parser ──────────────────────────────────────────────

    #[test]
    fn parse_gemini_approval_mode_yolo() {
        let cmd = ParsedCommand::parse(": gemini --approval-mode yolo fix a bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(
                    options
                        .gemini_extras()
                        .expect("gemini extras")
                        .approval_mode
                        .as_deref(),
                    Some("yolo")
                );
                assert_eq!(prompt, "fix a bug");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_approval_mode_invalid_rejected() {
        let err = ParsedCommand::parse(": gemini --approval-mode banana hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(msg.contains("--approval-mode"), "got: {}", msg);
                assert!(msg.contains("banana"), "got: {}", msg);
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_approval_mode_equals_syntax() {
        let cmd = ParsedCommand::parse(": gemini --approval-mode=plan hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert_eq!(
                    options
                        .gemini_extras()
                        .expect("gemini extras")
                        .approval_mode
                        .as_deref(),
                    Some("plan")
                );
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_fork_is_rejected_with_specific_error() {
        let err = ParsedCommand::parse(": gemini --fork --continue hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("gemini does not support --fork"),
                    "expected gemini-specific fork rejection; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_on_fork_is_rejected() {
        let err = ParsedCommand::parse(": gemini on --fork --continue", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("gemini does not support --fork"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_fork_still_accepted() {
        // Regression guard: the gemini-specific rejection must not leak
        // to other harnesses. `--fork` + `--resume <name>` is opencode's
        // canonical forking pattern.
        let cmd =
            ParsedCommand::parse(": opencode --fork --resume review keep going", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert!(
                    options.opencode_extras().expect("opencode extras").fork,
                    "opencode --fork must still parse"
                );
                assert_eq!(options.resume.as_deref(), Some("review"));
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn harness_options_summary_includes_approval_mode() {
        let opts = HarnessOptions {
            name: Some("auth".into()),
            extras: Some(HarnessExtras::Gemini(GeminiExtras {
                approval_mode: Some("yolo".into()),
            })),
            ..Default::default()
        };
        let summary = opts.summary();
        assert!(
            summary.contains("approval-mode=yolo"),
            "summary must show approval mode for visibility; got: {}",
            summary
        );
        assert!(summary.contains("name=auth"));
    }

    // ── Per-prompt flag tests ────────────────────────────────────────────────

    #[test]
    fn parse_opencode_title_flag() {
        let cmd = ParsedCommand::parse(": opencode --title mywork fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(
                    options
                        .opencode_extras()
                        .expect("opencode extras")
                        .title
                        .as_deref(),
                    Some("mywork")
                );
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_share_flag() {
        let cmd = ParsedCommand::parse(": opencode --share fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(options.opencode_extras().expect("opencode extras").share);
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_pure_flag() {
        let cmd = ParsedCommand::parse(": opencode --pure fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(options.opencode_extras().expect("opencode extras").pure);
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_fork_with_resume() {
        let cmd =
            ParsedCommand::parse(": opencode --resume auth --fork continue the work", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(options.opencode_extras().expect("opencode extras").fork);
                assert_eq!(options.resume.as_deref(), Some("auth"));
                assert_eq!(prompt, "continue the work");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_fork_without_resume_rejected() {
        let err = ParsedCommand::parse(": opencode --fork fix the bug", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("--fork requires"));
    }

    #[test]
    fn parse_opencode_continue_last() {
        // Bare `--continue` (next token is another flag `--share`) → continue_last = true
        let cmd = ParsedCommand::parse(": opencode --continue --share fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(options.continue_last, "continue_last should be true");
                assert!(options.resume.is_none(), "resume should be None");
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_continue_with_name() {
        // `--continue <name>` → resume = Some(name) (existing behavior preserved)
        let cmd = ParsedCommand::parse(": opencode --continue auth fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.resume.as_deref(), Some("auth"));
                assert!(!options.continue_last);
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_continue_last_with_name_rejected() {
        // `--continue` (bare, followed by `--share`) combined with `--name` is an error
        let err =
            ParsedCommand::parse(": opencode --name auth --continue --share fix the bug", ':')
                .unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        assert!(err.to_string().contains("Cannot use bare --continue"));
    }

    #[test]
    fn parse_opencode_fork_with_continue_last() {
        // `--continue --fork`: `--continue` sees `--fork` as next (a flag) → continue_last=true,
        // then `--fork` is also parsed → fork=true. Both together are valid.
        let cmd = ParsedCommand::parse(": opencode --continue --fork fix the bug", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(options.continue_last);
                assert!(options.opencode_extras().expect("opencode extras").fork);
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("Expected HarnessPrompt, got {:?}", other),
        }
    }

    // ── Native opencode form tests ─────────────────────────────────────────────

    #[test]
    fn parse_opencode_session_list_maps_to_sessions_subcommand() {
        let cmd = ParsedCommand::parse(": opencode session list", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Sessions),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_session_ls_maps_to_sessions_subcommand() {
        let cmd = ParsedCommand::parse(": opencode session ls", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Sessions),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_auth_list_maps_to_providers_subcommand() {
        let cmd = ParsedCommand::parse(": opencode auth list", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Providers),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_auth_ls_maps_to_providers_subcommand() {
        let cmd = ParsedCommand::parse(": opencode auth ls", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Providers),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_opencode_bare_session_is_blocked() {
        let err = ParsedCommand::parse(": opencode session", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(msg.contains("not available from chat"), "got: {}", msg);
    }

    #[test]
    fn parse_opencode_bare_auth_is_blocked() {
        // Verify via a freshly-named test (the old `parse_opencode_blocked_subcommand_bare_auth`
        // tests the same behavior; this one is the canonical name requested).
        let err = ParsedCommand::parse(": opencode auth", ':').unwrap_err();
        assert!(matches!(err, ParseError::InvalidHarnessOption(_)));
        let msg = err.to_string();
        assert!(msg.contains("not available from chat"), "got: {}", msg);
    }

    #[test]
    fn parse_opencode_stats_em_dash_days_normalized() {
        // iOS autocorrect: `—days` should become `--days` after em-dash normalization.
        let cmd = ParsedCommand::parse(": opencode stats \u{2014}days 1", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Stats),
                args: vec!["--days".into(), "1".into()],
            }
        );
    }

    #[test]
    fn parse_opencode_models_em_dash_flag_normalized() {
        // Fix 1.7: Only em-dash (U+2014) is normalized to `--`. En-dash (U+2013)
        // is intentionally preserved. `: opencode models –provider openrouter`
        // passes `–provider` through as-is (not normalized to `--provider`).
        let cmd =
            ParsedCommand::parse(": opencode models \u{2013}provider openrouter", ':').unwrap();
        assert_eq!(
            cmd,
            ParsedCommand::HarnessSubcommand {
                harness: HarnessKind::Opencode,
                subcommand: HarnessSubcommandKind::Opencode(OpencodeSubcommand::Models),
                args: vec!["\u{2013}provider".into(), "openrouter".into()],
            }
        );
    }

    // ── --model / --agent per-prompt flag tests ──────────────────────────────

    #[test]
    fn parse_opencode_model_flag_per_prompt() {
        let cmd =
            ParsedCommand::parse(": opencode --model ollama-cloud/glm-5.1 say hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                prompt,
                options,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.model.as_deref(), Some("ollama-cloud/glm-5.1"));
                assert_eq!(prompt, "say hi");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_agent_flag_per_prompt() {
        let cmd = ParsedCommand::parse(": opencode --agent build run bash ls", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                prompt,
                options,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(
                    options
                        .opencode_extras()
                        .expect("opencode extras")
                        .agent
                        .as_deref(),
                    Some("build")
                );
                assert_eq!(prompt, "run bash ls");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_on_with_model_flag() {
        let cmd = ParsedCommand::parse(": opencode on --model ollama-cloud/glm-5.1", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                harness,
                options,
                initial_prompt,
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(options.model.as_deref(), Some("ollama-cloud/glm-5.1"));
                assert!(initial_prompt.is_none());
            }
            other => panic!("expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_on_with_agent_flag() {
        let cmd = ParsedCommand::parse(": opencode on --agent build", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                harness, options, ..
            } => {
                assert_eq!(harness, HarnessKind::Opencode);
                assert_eq!(
                    options
                        .opencode_extras()
                        .expect("opencode extras")
                        .agent
                        .as_deref(),
                    Some("build")
                );
            }
            other => panic!("expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_model_and_agent_combined() {
        let cmd = ParsedCommand::parse(": opencode --model foo --agent bar do it", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.model.as_deref(), Some("foo"));
                assert_eq!(
                    options
                        .opencode_extras()
                        .expect("opencode extras")
                        .agent
                        .as_deref(),
                    Some("bar")
                );
                assert_eq!(prompt, "do it");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn harness_options_is_empty_false_when_agent_set() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Opencode(OpencodeExtras {
                agent: Some("build".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    #[test]
    fn harness_options_summary_includes_agent() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Opencode(OpencodeExtras {
                agent: Some("build".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(opts.summary(), "agent=build");
    }

    // ── Codex parser tests ─────────────────────────────────────────────────

    #[test]
    fn parse_codex_basic_prompt() {
        let cmd = ParsedCommand::parse(": codex what is 2+2", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness, prompt, ..
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(prompt, "what is 2+2");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_named_session() {
        let cmd = ParsedCommand::parse(": codex --name auth fix login", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(options.name.as_deref(), Some("auth"));
                assert_eq!(prompt, "fix login");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_resume_strict() {
        let cmd = ParsedCommand::parse(": codex --resume auth keep going", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(options.resume.as_deref(), Some("auth"));
                assert_eq!(prompt, "keep going");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_continue_last() {
        // Bare `--continue` requires the next token to be a flag (or absent)
        // to be interpreted as continue_last. Add `--sandbox read-only` so
        // the parser sees the flag boundary before the prompt.
        let cmd =
            ParsedCommand::parse(": codex --continue --sandbox read-only keep going", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert!(
                    options.continue_last,
                    "bare --continue followed by another flag should set continue_last"
                );
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .sandbox
                        .as_deref(),
                    Some("read-only")
                );
                assert_eq!(prompt, "keep going");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_sandbox_each_value_accepted() {
        for val in ["read-only", "workspace-write", "danger-full-access"] {
            let input = format!(": codex --sandbox {} hi", val);
            match ParsedCommand::parse(&input, ':').unwrap() {
                ParsedCommand::HarnessPrompt { options, .. } => {
                    assert_eq!(
                        options
                            .codex_extras()
                            .expect("codex extras")
                            .sandbox
                            .as_deref(),
                        Some(val),
                        "expected sandbox={}",
                        val
                    );
                }
                other => panic!("expected HarnessPrompt for `{}`, got {:?}", val, other),
            }
        }
    }

    #[test]
    fn parse_codex_sandbox_invalid_rejected() {
        let err = ParsedCommand::parse(": codex --sandbox banana hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(msg.contains("--sandbox"), "got: {}", msg);
                assert!(msg.contains("banana"), "got: {}", msg);
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_profile_passes_through() {
        let cmd = ParsedCommand::parse(": codex --profile teamsmall hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .profile
                        .as_deref(),
                    Some("teamsmall")
                );
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_approval_mode_rejected_cross_harness_redirect() {
        // F10: `--approval-mode` is codex-rejected at parse time. Codex 0.128's
        // approval/sandbox system lives behind `-s <sandbox>` and permission
        // profiles, not a `--approval-mode` flag. The chat-safe error
        // redirects to gemini; the deadlock-explanation wording lives on the
        // gemini path (verified by
        // `parse_gemini_approval_mode_on_request_rejected_with_deadlock_message`).
        let err = ParsedCommand::parse(": codex --approval-mode on-request hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--approval-mode") && msg.contains("gemini"),
                    "expected --approval-mode rejection pointing at gemini; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_approval_mode_on_request_rejected_with_deadlock_message() {
        // The deadlock-explanation path is now gemini-specific. This test
        // pins the helpful wording that was previously asserted on the codex
        // path before F10 routed `--approval-mode` to gemini-only.
        let err = ParsedCommand::parse(": gemini --approval-mode on-request hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("on-request") && msg.contains("deadlock"),
                    "expected deadlock-explaining error; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    // ── F10: cross-harness flag rejection ──────────────────────────────────
    //
    // The parser now rejects per-harness flags when used with the wrong
    // harness, instead of silently dropping them. These tests pin the new
    // behavior so a regression to silent-drop is caught.

    #[test]
    fn parse_claude_sandbox_rejected() {
        let err = ParsedCommand::parse(": claude --sandbox read-only hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--sandbox") && msg.contains("codex"),
                    "expected --sandbox rejection pointing at codex; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_claude_approval_mode_rejected() {
        let err = ParsedCommand::parse(": claude --approval-mode plan hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--approval-mode") && msg.contains("gemini"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_effort_rejected() {
        let err = ParsedCommand::parse(": codex --effort high hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--effort") && msg.contains("claude"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_agent_rejected() {
        let err = ParsedCommand::parse(": codex --agent build hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--agent") && msg.contains("opencode"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_opencode_sandbox_rejected() {
        let err = ParsedCommand::parse(": opencode --sandbox read-only hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--sandbox") && msg.contains("codex"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_max_turns_rejected() {
        let err = ParsedCommand::parse(": gemini --max-turns 5 hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--max-turns") && msg.contains("claude"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_add_dir_accepted() {
        // --add-dir is shared between claude and codex; both must accept.
        let cmd = ParsedCommand::parse(": codex --add-dir /tmp/a hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert_eq!(options.add_dirs.len(), 1);
                assert_eq!(options.add_dirs[0], PathBuf::from("/tmp/a"));
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_multiple_add_dir_threads_all_paths() {
        // Codex's --add-dir is repeatable. The shared add_dirs Vec<PathBuf>
        // must accumulate every value (a `push` mutation that no-ops the second
        // call would otherwise survive). (Test-engineer P1.)
        let cmd =
            ParsedCommand::parse(": codex --add-dir /tmp/a --add-dir /tmp/b hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                options, prompt, ..
            } => {
                assert_eq!(prompt, "hi");
                assert_eq!(
                    options.add_dirs,
                    vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
                );
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_gemini_add_dir_rejected() {
        // gemini does not accept --add-dir.
        let err = ParsedCommand::parse(": gemini --add-dir /tmp/a hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(msg.contains("--add-dir"), "got: {}", msg);
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_blocked_subcommands_all_rejected() {
        // F6+F7 update: `cloud`, `apply`, and `sessions` are NO LONGER on the
        // blocked list — `cloud <sub>` (with a recognized sub), `apply <id>`,
        // and bare `sessions` are now routed to HarnessSubcommand. Bare
        // `cloud` (with no recognized sub) still returns a chat-safe error
        // pointing at the valid forms; verified separately below.
        let blocked = [
            "login",
            "logout",
            "mcp",
            "mcp-server",
            "app",
            "app-server",
            "exec-server",
            "plugin",
            "completion",
            "features",
            "debug",
            "sandbox",
            "review",
            "resume",
            "fork",
        ];
        for kw in &blocked {
            let input = format!(": codex {}", kw);
            match ParsedCommand::parse(&input, ':') {
                Err(ParseError::InvalidHarnessOption(msg)) => {
                    assert!(
                        msg.contains("not available from chat") && msg.contains(kw),
                        "blocked codex keyword `{}` did not surface a chat-safe error; got: {}",
                        kw,
                        msg
                    );
                }
                other => panic!(
                    "blocked codex keyword `{}` should be rejected, got {:?}",
                    kw, other
                ),
            }
        }
    }

    #[test]
    fn parse_codex_resume_subcommand_redirects_to_flag_form() {
        let err = ParsedCommand::parse(": codex resume something", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("--resume <name>"),
                    "expected guidance toward the --resume flag; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_login_returns_terminal_only_message() {
        let err = ParsedCommand::parse(": codex login", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("auth must be run from your terminal"),
                    "expected auth/terminal guidance; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_bare_cloud_lists_safe_forms() {
        // Bare `: codex cloud` (no recognized sub) returns the form listing —
        // it's a discoverable error rather than a v1.1 deferral now.
        let err = ParsedCommand::parse(": codex cloud", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("cloud list")
                        && msg.contains("cloud status")
                        && msg.contains("cloud exec"),
                    "expected listing of safe cloud forms; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_unknown_cloud_sub_returns_listing() {
        // Unknown second word under `cloud` falls through to bare-cloud guidance.
        let err = ParsedCommand::parse(": codex cloud bananas", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("cloud list") && msg.contains("cloud exec"),
                    "expected cloud-form listing; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_sessions_routes_to_subcommand() {
        match ParsedCommand::parse(": codex sessions", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                args,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Sessions)
                );
                assert!(args.is_empty());
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_sessions_with_extra_args_rejected() {
        // `codex resume --all` (the underlying CLI form) takes no positional
        // arguments. Reject extras chat-side rather than passing through to a
        // confusing codex-side error. (Critic P2.)
        let err = ParsedCommand::parse(": codex sessions --limit 5", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("takes no arguments"),
                    "expected no-args rejection; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_apply_with_task_id_routes_to_subcommand() {
        match ParsedCommand::parse(": codex apply task_abc123", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                args,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Apply)
                );
                assert_eq!(args, vec!["task_abc123".to_string()]);
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_apply_without_task_id_rejected() {
        let err = ParsedCommand::parse(": codex apply", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("requires a task id") && msg.contains("codex apply"),
                    "expected task-id-required error; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_list_routes_with_no_args() {
        match ParsedCommand::parse(": codex cloud list", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                harness,
                subcommand,
                args,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::List
                    ))
                );
                assert!(args.is_empty());
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_list_with_limit_passes_args() {
        match ParsedCommand::parse(": codex cloud list --limit 5", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                subcommand, args, ..
            } => {
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::List
                    ))
                );
                assert_eq!(args, vec!["--limit".to_string(), "5".to_string()]);
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_status_with_id() {
        match ParsedCommand::parse(": codex cloud status task_xyz", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                subcommand, args, ..
            } => {
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::Status
                    ))
                );
                assert_eq!(args, vec!["task_xyz".to_string()]);
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_status_without_id_rejected() {
        let err = ParsedCommand::parse(": codex cloud status", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("requires a task id") && msg.contains("cloud status"),
                    "expected task-id-required error; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_diff_without_id_rejected() {
        let err = ParsedCommand::parse(": codex cloud diff", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("requires a task id") && msg.contains("cloud diff"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_apply_routes_with_id() {
        match ParsedCommand::parse(": codex cloud apply task_qq", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                subcommand, args, ..
            } => {
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::Apply
                    ))
                );
                assert_eq!(args, vec!["task_qq".to_string()]);
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_exec_passes_full_args() {
        // `cloud exec` requires --env upstream; we let codex surface the usage error
        // rather than gatekeeping. Multi-word query is preserved as separate tokens.
        match ParsedCommand::parse(": codex cloud exec --env env_main fix the auth bug", ':')
            .unwrap()
        {
            ParsedCommand::HarnessSubcommand {
                subcommand, args, ..
            } => {
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::Exec
                    ))
                );
                assert_eq!(
                    args,
                    vec![
                        "--env".to_string(),
                        "env_main".to_string(),
                        "fix".to_string(),
                        "the".to_string(),
                        "auth".to_string(),
                        "bug".to_string(),
                    ]
                );
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_cloud_exec_without_env_routes_to_codex() {
        // Intentional design: `--env` validation is delegated to the codex CLI
        // (which knows the valid set of environment IDs). Terminus routes the
        // call through and lets codex surface the missing-flag usage error so
        // the user sees codex's authoritative wording. This test pins that
        // permissive behavior so a future "tighten parser" pass doesn't
        // regress it without acknowledging the design call.
        match ParsedCommand::parse(": codex cloud exec fix the auth bug", ':').unwrap() {
            ParsedCommand::HarnessSubcommand {
                subcommand, args, ..
            } => {
                assert_eq!(
                    subcommand,
                    HarnessSubcommandKind::Codex(CodexSubcommand::Cloud(
                        CodexCloudSubcommand::Exec
                    ))
                );
                assert_eq!(
                    args,
                    vec![
                        "fix".to_string(),
                        "the".to_string(),
                        "auth".to_string(),
                        "bug".to_string(),
                    ]
                );
            }
            other => panic!("expected HarnessSubcommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_fork_flag_rejected_with_specific_error() {
        let err = ParsedCommand::parse(": codex --fork --continue hi", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("codex does not support --fork"),
                    "expected codex-specific fork rejection; got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_on_fork_flag_rejected() {
        let err = ParsedCommand::parse(": codex on --fork --continue", ':').unwrap_err();
        match err {
            ParseError::InvalidHarnessOption(msg) => {
                assert!(
                    msg.contains("codex does not support --fork"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidHarnessOption, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_em_dashed_flag_normalized() {
        let cmd = ParsedCommand::parse(": codex —name auth fix it", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(options.name.as_deref(), Some("auth"));
                assert_eq!(prompt, "fix it");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_on_with_flags() {
        let cmd =
            ParsedCommand::parse(": codex on --sandbox read-only --name review", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessOn {
                harness, options, ..
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .sandbox
                        .as_deref(),
                    Some("read-only")
                );
                assert_eq!(options.name.as_deref(), Some("review"));
            }
            other => panic!("expected HarnessOn, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_off() {
        match ParsedCommand::parse(": codex off", ':').unwrap() {
            ParsedCommand::HarnessOff { harness } => assert_eq!(harness, HarnessKind::Codex),
            other => panic!("expected HarnessOff, got {:?}", other),
        }
    }

    #[test]
    fn harness_options_summary_includes_sandbox_and_profile() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("workspace-write".into()),
                profile: Some("dev".into()),
            })),
            ..Default::default()
        };
        let summary = opts.summary();
        assert!(
            summary.contains("sandbox=workspace-write"),
            "got: {}",
            summary
        );
        assert!(summary.contains("profile=dev"), "got: {}", summary);
    }

    #[test]
    fn harness_options_is_empty_false_when_sandbox_set() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                sandbox: Some("read-only".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    #[test]
    fn harness_options_is_empty_false_when_profile_set() {
        let opts = HarnessOptions {
            extras: Some(HarnessExtras::Codex(CodexExtras {
                profile: Some("dev".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    #[test]
    fn parse_codex_combined_flags_sandbox_profile_model() {
        // Regression guard: combined parsing of all three codex-specific
        // flags together with a prompt. Individual flag tests pass but
        // combined parsing was untested.
        let cmd = ParsedCommand::parse(
            ": codex --sandbox read-only --profile dev --model gpt-5.4 fix the bug",
            ':',
        )
        .unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt {
                harness,
                options,
                prompt,
            } => {
                assert_eq!(harness, HarnessKind::Codex);
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .sandbox
                        .as_deref(),
                    Some("read-only")
                );
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .profile
                        .as_deref(),
                    Some("dev")
                );
                assert_eq!(options.model.as_deref(), Some("gpt-5.4"));
                assert_eq!(prompt, "fix the bug");
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_sandbox_equals_syntax() {
        let cmd = ParsedCommand::parse(": codex --sandbox=workspace-write hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .sandbox
                        .as_deref(),
                    Some("workspace-write")
                );
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }

    #[test]
    fn parse_codex_profile_equals_syntax() {
        let cmd = ParsedCommand::parse(": codex --profile=mydev hi", ':').unwrap();
        match cmd {
            ParsedCommand::HarnessPrompt { options, .. } => {
                assert_eq!(
                    options
                        .codex_extras()
                        .expect("codex extras")
                        .profile
                        .as_deref(),
                    Some("mydev")
                );
            }
            other => panic!("expected HarnessPrompt, got {:?}", other),
        }
    }
}
