use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub auth: AuthConfig,
    pub telegram: Option<TelegramConfig>,
    pub slack: Option<SlackConfig>,
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub blocklist: BlocklistConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub power: PowerConfig,
    /// Per-schema structured output configuration (`[schemas.<name>]` tables).
    #[serde(default)]
    pub schemas: HashMap<String, SchemaEntry>,
    /// Global structured output config (`[structured_output]` table).
    #[serde(default)]
    pub structured_output: StructuredOutputConfig,
    /// WebSocket socket server config (`[socket]` table).
    #[serde(default)]
    pub socket: SocketConfig,
    /// Harness session management config (`[harness]` table).
    #[serde(default)]
    pub harness: HarnessConfig,
}

// ─── Socket configuration ────────────────────────────────────────────────────

/// Configuration for the WebSocket bidirectional API.
///
/// Feature is opt-in: `enabled = false` by default. When enabled, terminus
/// listens on `ws://bind:port` (TLS terminated externally by a reverse proxy).
#[derive(Debug, Deserialize, Clone)]
pub struct SocketConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_socket_bind")]
    pub bind: String,
    #[serde(default = "default_socket_port")]
    pub port: u16,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_max_subscriptions_per_connection")]
    pub max_subscriptions_per_connection: usize,
    #[serde(default = "default_max_pending_requests")]
    pub max_pending_requests: usize,
    #[serde(default = "default_rate_limit_per_second")]
    pub rate_limit_per_second: f64,
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: f64,
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "default_ping_interval_secs")]
    pub ping_interval_secs: u64,
    #[serde(default = "default_pong_timeout_secs")]
    pub pong_timeout_secs: u64,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[allow(dead_code)] // reserved for broadcast receiver capacity tuning
    #[serde(default = "default_send_buffer_size")]
    pub send_buffer_size: usize,
    #[serde(default = "default_shutdown_drain_secs")]
    pub shutdown_drain_secs: u64,
    #[serde(default = "default_max_binary_bytes")]
    pub max_binary_bytes: usize,
    /// Hard cap on the in-memory subscription-restore map. LRU-evicted clients
    /// will restore no subscriptions on reconnect (equivalent to a fresh connect).
    #[serde(default = "default_max_subscription_store_entries")]
    pub max_subscription_store_entries: usize,
    /// Per-client named tokens for authentication.
    #[serde(default, rename = "client")]
    pub clients: Vec<SocketClient>,
}

/// A named client with a bearer token.
///
/// Manual `Debug` impl redacts the token, matching the pattern used for
/// `TelegramConfig`, `SlackConfig`, and `DiscordConfig`.
#[derive(Deserialize, Clone)]
pub struct SocketClient {
    pub name: String,
    pub token: String,
}

impl std::fmt::Debug for SocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketClient")
            .field("name", &self.name)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

fn default_socket_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_socket_port() -> u16 {
    7645
}
fn default_max_connections() -> usize {
    16
}
fn default_max_subscriptions_per_connection() -> usize {
    8
}
fn default_max_pending_requests() -> usize {
    32
}
fn default_rate_limit_per_second() -> f64 {
    20.0
}
fn default_rate_limit_burst() -> f64 {
    60.0
}
fn default_max_message_bytes() -> usize {
    1_048_576
}
fn default_ping_interval_secs() -> u64 {
    30
}
fn default_pong_timeout_secs() -> u64 {
    10
}
fn default_idle_timeout_secs() -> u64 {
    300
}
fn default_send_buffer_size() -> usize {
    1024
}
fn default_shutdown_drain_secs() -> u64 {
    30
}
fn default_max_binary_bytes() -> usize {
    10_485_760 // 10 MiB
}
fn default_max_subscription_store_entries() -> usize {
    100
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_socket_bind(),
            port: default_socket_port(),
            max_connections: default_max_connections(),
            max_subscriptions_per_connection: default_max_subscriptions_per_connection(),
            max_pending_requests: default_max_pending_requests(),
            rate_limit_per_second: default_rate_limit_per_second(),
            rate_limit_burst: default_rate_limit_burst(),
            max_message_bytes: default_max_message_bytes(),
            ping_interval_secs: default_ping_interval_secs(),
            pong_timeout_secs: default_pong_timeout_secs(),
            idle_timeout_secs: default_idle_timeout_secs(),
            send_buffer_size: default_send_buffer_size(),
            shutdown_drain_secs: default_shutdown_drain_secs(),
            max_binary_bytes: default_max_binary_bytes(),
            max_subscription_store_entries: default_max_subscription_store_entries(),
            clients: Vec::new(),
        }
    }
}

/// Configuration for a single named schema.
///
/// ```toml
/// [schemas.todos]
/// schema = '{ "type": "object", ... }'
/// webhook = "https://my-server/hooks/todos"
/// webhook_secret_env = "TERMINUS_TODOS_SECRET"
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct SchemaEntry {
    /// Inline JSON Schema as a TOML multi-line string.  Parsed at startup.
    pub schema: String,
    /// Webhook URL to POST structured output to, if set.
    pub webhook: Option<String>,
    /// Name of an environment variable containing the HMAC-SHA256 secret.
    /// Required iff `webhook` is set.
    pub webhook_secret_env: Option<String>,
}

/// Global structured output configuration (`[structured_output]` table).
#[derive(Debug, Deserialize, Clone)]
pub struct StructuredOutputConfig {
    /// Webhook request timeout in milliseconds.  Default: 5000.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Directory for the durable delivery queue.
    /// Default: `~/.local/share/terminus/queue`.
    #[serde(default = "default_queue_dir")]
    pub queue_dir: PathBuf,
    /// Maximum age (in hours) before a queued job is abandoned.
    /// `0` means retry forever (the default).
    #[serde(default)]
    pub max_retry_age_hours: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

fn default_queue_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("terminus")
        .join("queue")
}

/// Configuration for harness session management.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HarnessConfig {
    /// Maximum named harness sessions before LRU eviction (default: 50).
    pub max_named_sessions: Option<usize>,
    /// Opencode CLI-subprocess harness configuration.  Optional — when absent,
    /// the harness resolves `opencode` from PATH and inherits the user's own
    /// CLI config (default model, agent, provider, auth).
    #[serde(default)]
    pub opencode: Option<OpencodeConfig>,
    /// Gemini CLI-subprocess harness configuration.  Optional — when absent,
    /// the harness resolves `gemini` from PATH and inherits the user's own
    /// CLI config.
    #[serde(default)]
    pub gemini: Option<GeminiConfig>,
    /// Codex CLI-subprocess harness configuration.  Optional — when absent,
    /// the harness resolves `codex` from PATH and inherits the user's own
    /// CLI config (`~/.codex/config.toml`, profiles, auth).
    #[serde(default)]
    pub codex: Option<CodexConfig>,
}

/// Configuration for the Opencode CLI-subprocess harness.
///
/// All fields are optional. When a field is `None`, the corresponding CLI flag
/// is omitted from `opencode run ...` and the binary falls back to the user's
/// own opencode config defaults (from `~/.config/opencode` etc).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpencodeConfig {
    /// Override path to the `opencode` binary. When `None`, resolves via PATH.
    pub binary_path: Option<PathBuf>,
    /// Optional model override in `provider/model` format
    /// (e.g. `"openrouter/anthropic/claude-haiku-4-5"`). Passed via `-m`.
    pub model: Option<String>,
    /// Optional agent override (e.g. `"build"`). Passed via `--agent`.
    pub agent: Option<String>,
}

/// Configuration for the Gemini CLI-subprocess harness.
///
/// All fields are optional. When a field is `None`, the corresponding CLI flag
/// is omitted from `gemini ...` and the binary falls back to the user's own
/// gemini-cli config defaults.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GeminiConfig {
    /// Override path to the `gemini` binary. When `None`, resolves via PATH.
    pub binary_path: Option<PathBuf>,
    /// Optional model override (e.g. `"flash"`, `"flash-lite"`, `"pro"`).
    /// Passed via `-m`.
    pub model: Option<String>,
    /// Optional approval mode (`default` | `auto_edit` | `yolo` | `plan`).
    /// Passed via `--approval-mode`.
    pub approval_mode: Option<String>,
}

/// Configuration for the Codex CLI-subprocess harness.
///
/// All fields are optional. When a field is `None`, the corresponding CLI flag
/// is omitted from `codex exec ...` and the binary falls back to the user's
/// own config (`~/.codex/config.toml`, active profile, ChatGPT/API auth).
///
/// **Defaults applied unconditionally** by terminus regardless of these
/// fields (so the harness never deadlocks waiting on TTY approval prompts):
/// - `--full-auto` (sandboxed automatic execution; codex 0.125.0 removed the
///   prior `--ask-for-approval` flag).
/// - `--skip-git-repo-check` (terminus's tmux cwd may not be a git repo).
/// - `--ephemeral` when no named/resumed session is in play.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexConfig {
    /// Override path to the `codex` binary. When `None`, resolves via PATH.
    pub binary_path: Option<PathBuf>,
    /// Optional model override (e.g. `"gpt-5.4"`, `"gpt-5.3-codex"`).
    /// Passed via `-m`. When `None`, codex uses its own default.
    pub model: Option<String>,
    /// Optional codex profile name. Passed via `-p / --profile <name>`.
    /// Selects a `[profiles.<name>]` block from `~/.codex/config.toml`.
    pub profile: Option<String>,
    /// Optional sandbox policy (`read-only` | `workspace-write` |
    /// `danger-full-access`). Passed via `-s / --sandbox`. When `None`,
    /// codex applies its own default (typically `workspace-write`).
    pub sandbox: Option<String>,
    /// When `true`, terminus passes `--ignore-user-config` to codex (skips
    /// `~/.codex/config.toml`, including profiles). Defense-in-depth against
    /// future codex releases that might add profile fields re-introducing
    /// approval prompts. Defaults to `false` (respect user's profile).
    #[serde(default)]
    pub ignore_user_config: bool,
}

impl Default for StructuredOutputConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            queue_dir: default_queue_dir(),
            max_retry_age_hours: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PowerConfig {
    #[serde(default = "power_enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub stayawake_on_battery: bool,
    #[serde(default)]
    pub state_file: Option<std::path::PathBuf>,
}

fn power_enabled_default() -> bool {
    true
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            enabled: power_enabled_default(),
            stayawake_on_battery: false,
            state_file: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AuthConfig {
    pub telegram_user_id: Option<u64>,
    pub slack_user_id: Option<String>,
    pub discord_user_id: Option<u64>,
}

#[derive(Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
    pub channel_id: String,
}

impl fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &"[REDACTED]")
            .field("app_token", &"[REDACTED]")
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

#[derive(Deserialize, Clone)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub guild_id: Option<u64>,
    pub channel_id: Option<u64>,
}

impl DiscordConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.channel_id.is_some() && self.guild_id.is_none() {
            anyhow::bail!("discord.channel_id requires discord.guild_id to be set");
        }
        Ok(())
    }
}

impl fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("bot_token", &"[REDACTED]")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct BlocklistConfig {
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamingConfig {
    #[serde(default = "default_edit_throttle_ms")]
    pub edit_throttle_ms: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_offline_buffer_max_bytes")]
    pub offline_buffer_max_bytes: usize,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            edit_throttle_ms: default_edit_throttle_ms(),
            poll_interval_ms: default_poll_interval_ms(),
            chunk_size: default_chunk_size(),
            offline_buffer_max_bytes: default_offline_buffer_max_bytes(),
            max_sessions: default_max_sessions(),
        }
    }
}

fn default_edit_throttle_ms() -> u64 {
    2000
}
fn default_poll_interval_ms() -> u64 {
    250
}
fn default_chunk_size() -> usize {
    4000
}
fn default_offline_buffer_max_bytes() -> usize {
    1_048_576
}
fn default_max_sessions() -> usize {
    10
}

#[derive(Debug, Deserialize)]
pub struct CommandsConfig {
    #[serde(default = "default_trigger")]
    pub trigger: char,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            trigger: default_trigger(),
        }
    }
}

fn default_trigger() -> char {
    ':'
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn telegram_enabled(&self) -> bool {
        self.telegram.is_some() && self.auth.telegram_user_id.is_some()
    }

    pub fn slack_enabled(&self) -> bool {
        self.slack.is_some() && self.auth.slack_user_id.is_some()
    }

    pub fn discord_enabled(&self) -> bool {
        self.discord.is_some() && self.auth.discord_user_id.is_some()
    }

    pub fn socket_enabled(&self) -> bool {
        self.socket.enabled && !self.socket.clients.is_empty()
    }

    fn validate(&self) -> Result<()> {
        let has_chat_platform =
            self.telegram_enabled() || self.slack_enabled() || self.discord_enabled();
        if !has_chat_platform && !self.socket_enabled() {
            anyhow::bail!(
                "At least one platform must be configured. \
                 Add a [telegram], [slack], or [discord] section, or enable [socket] \
                 with at least one [[socket.client]], to terminus.toml."
            );
        }

        if let Some(ref tg) = self.telegram {
            if tg.bot_token.is_empty() || tg.bot_token == "YOUR_BOT_TOKEN_HERE" {
                anyhow::bail!("[telegram] section present but bot_token is not set");
            }
            if self.auth.telegram_user_id.is_none() {
                anyhow::bail!("[telegram] section present but auth.telegram_user_id is not set");
            }
        }

        if let Some(ref sl) = self.slack {
            if sl.bot_token.is_empty() || sl.bot_token.starts_with("xoxb-YOUR") {
                anyhow::bail!("[slack] section present but bot_token is not set");
            }
            if sl.app_token.is_empty() || sl.app_token.starts_with("xapp-YOUR") {
                anyhow::bail!("[slack] section present but app_token is not set");
            }
            if self.auth.slack_user_id.is_none() {
                anyhow::bail!("[slack] section present but auth.slack_user_id is not set");
            }
        }

        if let Some(ref dc) = self.discord {
            dc.validate()?;
            if dc.bot_token.is_empty() || dc.bot_token == "YOUR_BOT_TOKEN_HERE" {
                anyhow::bail!("[discord] section present but bot_token is not set");
            }
            if self.auth.discord_user_id.is_none() {
                anyhow::bail!("[discord] section present but auth.discord_user_id is not set");
            }
        }

        const SAFE_TRIGGERS: &[char] = &[
            ':', '!', '>', ';', '.', ',', '@', '~', '^', '-', '+', '=', '|', '%', '?',
        ];
        if !SAFE_TRIGGERS.contains(&self.commands.trigger) {
            anyhow::bail!(
                "commands.trigger must be one of {:?}, got {:?}",
                SAFE_TRIGGERS,
                self.commands.trigger
            );
        }

        if self.streaming.poll_interval_ms == 0 {
            anyhow::bail!("streaming.poll_interval_ms must be > 0");
        }
        if self.streaming.chunk_size == 0 {
            anyhow::bail!("streaming.chunk_size must be > 0");
        }

        // Validate schema entries.
        for (name, entry) in &self.schemas {
            // JSON Schema string must be valid JSON.
            serde_json::from_str::<serde_json::Value>(&entry.schema).with_context(|| {
                format!(
                    "[schemas.{}] schema field is not valid JSON: check the TOML multi-line string",
                    name
                )
            })?;

            // webhook requires webhook_secret_env.
            if entry.webhook.is_some() && entry.webhook_secret_env.is_none() {
                anyhow::bail!(
                    "[schemas.{}] webhook is set but webhook_secret_env is missing; \
                     set webhook_secret_env to the name of an env var containing the HMAC secret",
                    name
                );
            }

            // webhook URL must use https:// — plain http leaks the signature
            // and full structured-output body over the wire.
            if let Some(ref url) = entry.webhook {
                if !url.starts_with("https://") {
                    anyhow::bail!(
                        "[schemas.{}] webhook URL must use https:// to protect HMAC \
                         signature and body in transit (got: {})",
                        name,
                        url
                    );
                }
            }

            // webhook_secret_env requires the env var to be resolvable.
            // (Actual loading into Secret<Vec<u8>> happens in SchemaRegistry::from_config.)
            if let Some(ref env_var) = entry.webhook_secret_env {
                if entry.webhook.is_some() {
                    std::env::var(env_var).with_context(|| {
                        format!(
                            "[schemas.{}] webhook_secret_env refers to env var '{}' which is not set",
                            name, env_var
                        )
                    })?;
                }
            }
        }

        // ── Socket validation ────────────────────────────────────────────────
        if self.socket.enabled {
            // Validate bind address is parseable
            if self.socket.bind.parse::<std::net::IpAddr>().is_err() {
                anyhow::bail!(
                    "[socket] bind '{}' is not a valid IP address",
                    self.socket.bind
                );
            }
            if self.socket.max_connections < 1 {
                anyhow::bail!("[socket] max_connections must be >= 1");
            }
            if self.socket.max_subscriptions_per_connection < 1 {
                anyhow::bail!("[socket] max_subscriptions_per_connection must be >= 1");
            }
            if self.socket.max_pending_requests < 1 {
                anyhow::bail!("[socket] max_pending_requests must be >= 1");
            }
            if self.socket.max_message_bytes < 1024 {
                anyhow::bail!("[socket] max_message_bytes must be >= 1024");
            }
            if self.socket.ping_interval_secs == 0 {
                anyhow::bail!("[socket] ping_interval_secs must be > 0");
            }
            if self.socket.pong_timeout_secs == 0 {
                anyhow::bail!("[socket] pong_timeout_secs must be > 0");
            }
            if self.socket.idle_timeout_secs == 0 {
                anyhow::bail!("[socket] idle_timeout_secs must be > 0");
            }
            if self.socket.shutdown_drain_secs == 0 {
                anyhow::bail!("[socket] shutdown_drain_secs must be > 0");
            }
            if self.socket.max_binary_bytes < 1024 {
                anyhow::bail!("[socket] max_binary_bytes must be >= 1024");
            }
            if self.socket.rate_limit_per_second <= 0.0 {
                anyhow::bail!("[socket] rate_limit_per_second must be > 0");
            }
            if self.socket.rate_limit_burst <= 0.0 {
                anyhow::bail!("[socket] rate_limit_burst must be > 0");
            }
            if self.socket.clients.is_empty() {
                anyhow::bail!(
                    "[socket] enabled but no [[socket.client]] entries configured; \
                     add at least one client with name and token"
                );
            }
            // Check for empty/short tokens and duplicate client names.
            let mut seen_names = std::collections::HashSet::new();
            for client in &self.socket.clients {
                if client.name.is_empty() {
                    anyhow::bail!("[[socket.client]] name must not be empty");
                }
                if client.token.len() < 32 {
                    anyhow::bail!(
                        "[[socket.client]] '{}' token must be >= 32 characters for security \
                         (got {} characters)",
                        client.name,
                        client.token.len()
                    );
                }
                if !seen_names.insert(&client.name) {
                    anyhow::bail!(
                        "[[socket.client]] duplicate name '{}'; each client must have a unique name",
                        client.name
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Helper to write a TOML string to a temp file and load it.
    fn load_from_str(toml_str: &str) -> anyhow::Result<Config> {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_str.as_bytes()).unwrap();
        Config::load(f.path())
    }

    #[test]
    fn discord_dm_only_config_valid() {
        let toml = r#"
[auth]
discord_user_id = 123456789

[discord]
bot_token = "test-token"

[blocklist]
patterns = []
"#;
        let config = load_from_str(toml).expect("discord DM-only config should be valid");
        assert!(config.discord_enabled());
    }

    #[test]
    fn discord_guild_channel_config_valid() {
        let toml = r#"
[auth]
discord_user_id = 123456789

[discord]
bot_token = "test-token"
guild_id = 111111111
channel_id = 222222222

[blocklist]
patterns = []
"#;
        load_from_str(toml).expect("discord guild+channel config should be valid");
    }

    #[test]
    fn discord_channel_without_guild_fails() {
        let toml = r#"
[auth]
discord_user_id = 123456789

[discord]
bot_token = "test-token"
channel_id = 222222222

[blocklist]
patterns = []
"#;
        let err = load_from_str(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("discord.channel_id requires discord.guild_id"),
            "expected guild_id error, got: {}",
            err
        );
    }

    #[test]
    fn discord_only_install_passes_validation() {
        let toml = r#"
[auth]
discord_user_id = 123456789

[discord]
bot_token = "test-token"

[blocklist]
patterns = []
"#;
        load_from_str(toml).expect("discord-only install should pass validation");
    }

    #[test]
    fn discord_enabled_requires_both_section_and_user_id() {
        // Section present, user_id missing
        let toml = r#"
[auth]
telegram_user_id = 99

[telegram]
bot_token = "tg-token"

[discord]
bot_token = "test-token"

[blocklist]
patterns = []
"#;
        let config = load_from_str(toml);
        // Should fail because [discord] present but no discord_user_id
        assert!(
            config.is_err(),
            "should fail when discord section present but no user_id"
        );

        // user_id present, section missing — discord_enabled returns false
        let toml2 = r#"
[auth]
telegram_user_id = 99
discord_user_id = 123

[telegram]
bot_token = "tg-token"

[blocklist]
patterns = []
"#;
        let config2 = load_from_str(toml2).expect("should load fine without [discord] section");
        assert!(
            !config2.discord_enabled(),
            "discord_enabled should be false without [discord] section"
        );
    }

    // ── Schema validation tests ──────────────────────────────────────────────

    fn minimal_telegram_toml_with_schemas(extra: &str) -> String {
        format!(
            r#"
[auth]
telegram_user_id = 99

[telegram]
bot_token = "tg-token"

[blocklist]
patterns = []

{}
"#,
            extra
        )
    }

    #[test]
    fn invalid_json_schema_fails_load() {
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = "{ not valid json }"
"#,
        );
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("[schemas.todos]"),
            "Error should cite [schemas.todos], got: {}",
            err
        );
    }

    #[test]
    fn webhook_without_secret_env_fails_load() {
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = '{"type": "object"}'
webhook = "https://example.com/hook"
"#,
        );
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("webhook_secret_env is missing"),
            "Error should mention webhook_secret_env, got: {}",
            err
        );
    }

    /// Non-https:// webhook URLs are rejected.  Leaks the HMAC signature and
    /// the entire structured-output body over the network to any on-path
    /// observer.
    #[test]
    fn http_webhook_url_fails_validation() {
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = '{"type": "object"}'
webhook = "http://example.com/hook"
webhook_secret_env = "IRRELEVANT"
"#,
        );
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("must use https://"),
            "Error should reject http://, got: {}",
            err
        );
    }

    #[test]
    #[serial(env_mutation)]
    fn missing_webhook_secret_env_fails_load() {
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = '{"type": "object"}'
webhook = "https://example.com/hook"
webhook_secret_env = "TERMINUS_NONEXISTENT_ENV_VAR_XYZ123"
"#,
        );
        // Make sure the env var is not set
        std::env::remove_var("TERMINUS_NONEXISTENT_ENV_VAR_XYZ123");
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("TERMINUS_NONEXISTENT_ENV_VAR_XYZ123"),
            "Error should cite the env var name, got: {}",
            err
        );
    }

    #[test]
    fn valid_schema_without_webhook_loads() {
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = '{"type": "object", "properties": {"todos": {"type": "array"}}}'
"#,
        );
        let config = load_from_str(&toml).expect("Valid schema without webhook should load");
        assert!(config.schemas.contains_key("todos"));
    }

    #[test]
    #[serial(env_mutation)]
    fn valid_schema_with_webhook_and_set_env_var_loads() {
        std::env::set_var("TERMINUS_TESTS_WEBHOOK_SECRET", "mysecret");
        let toml = minimal_telegram_toml_with_schemas(
            r#"
[schemas.todos]
schema = '{"type": "object"}'
webhook = "https://example.com/hook"
webhook_secret_env = "TERMINUS_TESTS_WEBHOOK_SECRET"
"#,
        );
        let config = load_from_str(&toml).expect("Valid schema with set env var should load");
        assert!(config.schemas.contains_key("todos"));
        std::env::remove_var("TERMINUS_TESTS_WEBHOOK_SECRET");
    }

    // ── Socket config validation tests ──────────────────────────────────────

    fn socket_only_toml(extra: &str) -> String {
        format!(
            r#"
[auth]

[blocklist]
patterns = []

[socket]
enabled = true
{}

[[socket.client]]
name = "agent-a"
token = "tk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#,
            extra
        )
    }

    #[test]
    fn socket_only_deployment_passes_validation() {
        let toml = socket_only_toml("");
        let config = load_from_str(&toml).expect("socket-only config should be valid");
        assert!(config.socket_enabled());
        assert!(!config.telegram_enabled());
    }

    #[test]
    fn socket_only_without_auth_or_blocklist_loads() {
        // Socket-only deployment with the minimum viable TOML: no [auth] and no
        // [blocklist]. Both sections default to empty.
        let toml = r#"
[socket]
enabled = true

[[socket.client]]
name = "agent-a"
token = "tk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        let config =
            load_from_str(toml).expect("socket-only without [auth]/[blocklist] should load");
        assert!(config.socket_enabled());
        assert!(!config.telegram_enabled());
        assert!(!config.slack_enabled());
        assert!(!config.discord_enabled());
        assert!(config.blocklist.patterns.is_empty());
    }

    #[test]
    fn telegram_without_auth_section_still_fails() {
        // Regression: [telegram] present but [auth] section entirely omitted —
        // validation must still reject (defaulted auth leaves telegram_enabled
        // false, which trips the "at least one platform" guard).
        let toml = r#"
[telegram]
bot_token = "tg-token"
"#;
        let err = load_from_str(toml).expect_err("telegram without auth must fail");
        assert!(
            err.to_string().contains("At least one platform"),
            "expected platform-required error, got: {}",
            err
        );
    }

    #[test]
    fn socket_zero_max_connections_fails() {
        let toml = socket_only_toml("max_connections = 0");
        let err = load_from_str(&toml).unwrap_err();
        assert!(err.to_string().contains("max_connections"), "got: {}", err);
    }

    #[test]
    fn socket_zero_max_pending_fails() {
        let toml = socket_only_toml("max_pending_requests = 0");
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("max_pending_requests"),
            "got: {}",
            err
        );
    }

    #[test]
    fn socket_zero_ping_interval_fails() {
        let toml = socket_only_toml("ping_interval_secs = 0");
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("ping_interval_secs"),
            "got: {}",
            err
        );
    }

    #[test]
    fn socket_small_max_message_bytes_fails() {
        let toml = socket_only_toml("max_message_bytes = 512");
        let err = load_from_str(&toml).unwrap_err();
        assert!(
            err.to_string().contains("max_message_bytes"),
            "got: {}",
            err
        );
    }

    #[test]
    fn socket_invalid_bind_address_fails() {
        let toml = socket_only_toml(r#"bind = "not-an-ip""#);
        let err = load_from_str(&toml).unwrap_err();
        assert!(err.to_string().contains("not a valid IP"), "got: {}", err);
    }

    #[test]
    fn socket_short_token_fails() {
        let toml = r#"
[auth]

[blocklist]
patterns = []

[socket]
enabled = true

[[socket.client]]
name = "agent-a"
token = "short"
"#;
        let err = load_from_str(toml).unwrap_err();
        assert!(err.to_string().contains("32 characters"), "got: {}", err);
    }

    #[test]
    fn socket_duplicate_client_name_fails() {
        let toml = r#"
[auth]

[blocklist]
patterns = []

[socket]
enabled = true

[[socket.client]]
name = "agent-a"
token = "tk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[socket.client]]
name = "agent-a"
token = "tk_live_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#;
        let err = load_from_str(toml).unwrap_err();
        assert!(err.to_string().contains("duplicate name"), "got: {}", err);
    }

    #[test]
    fn socket_empty_client_name_fails() {
        let toml = r#"
[auth]

[blocklist]
patterns = []

[socket]
enabled = true

[[socket.client]]
name = ""
token = "tk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        let err = load_from_str(toml).unwrap_err();
        assert!(
            err.to_string().contains("name must not be empty"),
            "got: {}",
            err
        );
    }

    #[test]
    fn socket_disabled_by_default() {
        let toml = r#"
[auth]
telegram_user_id = 99

[telegram]
bot_token = "tg-token"

[blocklist]
patterns = []
"#;
        let config = load_from_str(toml).expect("config without [socket] should load");
        assert!(!config.socket_enabled());
    }
}
