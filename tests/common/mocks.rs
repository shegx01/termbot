//! Mock implementations for integration tests.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use terminus::chat_adapters::{ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType};

/// Recorded interaction state from a `MockPlatform`. Use
/// `MockPlatform::snapshot().await` to get a copy without holding the lock.
#[derive(Default, Debug, Clone)]
pub struct MockPlatformState {
    /// `(chat_id, text)` pairs in the order `send_message` was called.
    pub messages_sent: Vec<(String, String)>,
    /// Number of times `pause()` was called.
    pub pause_count: usize,
    /// Number of times `resume()` was called.
    pub resume_count: usize,
}

/// Test double for `ChatPlatform`. Records every `send_message`, `pause`,
/// and `resume` call. Configurable to block `send_message` indefinitely so
/// the inline-prefix fallback path can be exercised in tests.
pub struct MockPlatform {
    ptype: PlatformType,
    state: Arc<Mutex<MockPlatformState>>,
    block_sends: bool,
}

impl MockPlatform {
    /// Telegram-typed mock that successfully completes `send_message` calls.
    pub fn telegram() -> Arc<Self> {
        Self::with_type(PlatformType::Telegram, false)
    }

    /// Slack-typed mock.
    pub fn slack() -> Arc<Self> {
        Self::with_type(PlatformType::Slack, false)
    }

    /// Discord-typed mock.
    pub fn discord() -> Arc<Self> {
        Self::with_type(PlatformType::Discord, false)
    }

    /// Telegram-typed mock whose `send_message` blocks forever, used to
    /// exercise the banner-ack timeout + inline-prefix fallback.
    pub fn telegram_blocking() -> Arc<Self> {
        Self::with_type(PlatformType::Telegram, true)
    }

    fn with_type(ptype: PlatformType, block_sends: bool) -> Arc<Self> {
        Arc::new(Self {
            ptype,
            state: Arc::new(Mutex::new(MockPlatformState::default())),
            block_sends,
        })
    }

    /// Take a snapshot of the recorded interactions.
    pub async fn snapshot(&self) -> MockPlatformState {
        self.state.lock().await.clone()
    }

    /// Convenience: return the pause/resume counts only.
    pub async fn pause_resume_counts(&self) -> (usize, usize) {
        let s = self.state.lock().await;
        (s.pause_count, s.resume_count)
    }
}

#[async_trait]
impl ChatPlatform for MockPlatform {
    async fn start(&self, _cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        Ok(())
    }

    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        if self.block_sends {
            std::future::pending::<()>().await;
        }
        self.state
            .lock()
            .await
            .messages_sent
            .push((chat_id.to_string(), text.to_string()));
        Ok(match self.ptype {
            PlatformType::Telegram => PlatformMessageId::Telegram(1),
            PlatformType::Slack => PlatformMessageId::Slack("1.0".to_string()),
            PlatformType::Discord => PlatformMessageId::Discord(1),
        })
    }

    async fn edit_message(
        &self,
        _msg_id: &PlatformMessageId,
        _chat_id: &str,
        _text: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn send_photo(
        &self,
        _data: &[u8],
        _filename: &str,
        _caption: Option<&str>,
        _chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        Ok(match self.ptype {
            PlatformType::Telegram => PlatformMessageId::Telegram(1),
            PlatformType::Slack => PlatformMessageId::Slack("1.0".to_string()),
            PlatformType::Discord => PlatformMessageId::Discord(1),
        })
    }

    async fn send_document(
        &self,
        _data: &[u8],
        _filename: &str,
        _caption: Option<&str>,
        _chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        Ok(match self.ptype {
            PlatformType::Telegram => PlatformMessageId::Telegram(1),
            PlatformType::Slack => PlatformMessageId::Slack("1.0".to_string()),
            PlatformType::Discord => PlatformMessageId::Discord(1),
        })
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn platform_type(&self) -> PlatformType {
        self.ptype
    }

    async fn pause(&self) {
        self.state.lock().await.pause_count += 1;
    }

    async fn resume(&self) {
        self.state.lock().await.resume_count += 1;
    }
}
