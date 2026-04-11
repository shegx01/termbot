use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub auth: AuthConfig,
    pub telegram: Option<TelegramConfig>,
    pub slack: Option<SlackConfig>,
    pub blocklist: BlocklistConfig,
    pub streaming: StreamingConfig,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    pub telegram_user_id: Option<u64>,
    pub slack_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
}

#[derive(Debug, Deserialize)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
    pub channel_id: String,
}

#[derive(Debug, Deserialize)]
pub struct BlocklistConfig {
    pub patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamingConfig {
    pub edit_throttle_ms: u64,
    pub poll_interval_ms: u64,
    pub chunk_size: usize,
    pub offline_buffer_max_bytes: usize,
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

        if self.streaming.poll_interval_ms == 0 {
            anyhow::bail!("streaming.poll_interval_ms must be > 0");
        }
        if self.streaming.chunk_size == 0 {
            anyhow::bail!("streaming.chunk_size must be > 0");
        }
        Ok(())
    }
}
