use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub auth: AuthConfig,
    pub telegram: Option<TelegramConfig>,
    pub slack: Option<SlackConfig>,
    pub blocklist: BlocklistConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub power: PowerConfig,
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

    fn validate(&self) -> Result<()> {
        if !self.telegram_enabled() && !self.slack_enabled() {
            anyhow::bail!(
                "At least one platform must be configured. \
                 Add a [telegram] or [slack] section to termbot.toml."
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
        Ok(())
    }
}
