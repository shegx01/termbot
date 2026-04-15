use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub auth: AuthConfig,
    pub telegram: Option<TelegramConfig>,
    pub slack: Option<SlackConfig>,
    pub discord: Option<DiscordConfig>,
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct BlocklistConfig {
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

    fn validate(&self) -> Result<()> {
        if !self.telegram_enabled() && !self.slack_enabled() && !self.discord_enabled() {
            anyhow::bail!(
                "At least one platform must be configured. \
                 Add a [telegram], [slack], or [discord] section to terminus.toml."
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
}
