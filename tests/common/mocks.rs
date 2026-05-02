//! Mock implementations for integration tests.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use terminus::chat_adapters::{ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType};

/// Ordered event log entry for `MockPlatform` — used by tests that need to
/// assert on the order in which `pause`, `send_message`, and `resume` fire
/// (counters alone can't prove ordering, since `pause=1, send=1, resume=1`
/// is satisfied by any permutation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEvent {
    Paused,
    Sent { chat_id: String, text: String },
    Edited { chat_id: String, text: String },
    PhotoSent { chat_id: String, filename: String },
    DocumentSent { chat_id: String, filename: String },
    Resumed,
}

/// Recorded interaction state from a `MockPlatform`. `event_log` carries
/// the strict order in which calls happened; the count fields are
/// convenience derivations that callers don't have to recompute.
#[derive(Default, Debug, Clone)]
pub struct MockPlatformState {
    /// Chronological log of every recorded interaction.
    pub event_log: Vec<MockEvent>,
    /// `(chat_id, text)` pairs in the order `send_message` was called.
    /// Convenience accessor — same data as `event_log` filtered to `Sent`.
    pub messages_sent: Vec<(String, String)>,
    /// Number of times `pause()` was called.
    pub pause_count: usize,
    /// Number of times `resume()` was called.
    pub resume_count: usize,
}

/// Test double for `ChatPlatform`. Records every `send_message`, `pause`,
/// `resume`, `edit_message`, `send_photo`, and `send_document` call
/// (R4-P1 — full surface so future tests can assert on richer paths).
/// Configurable to block `send_message` indefinitely so the inline-prefix
/// fallback path can be exercised in tests.
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
        let mut s = self.state.lock().await;
        s.event_log.push(MockEvent::Sent {
            chat_id: chat_id.to_string(),
            text: text.to_string(),
        });
        s.messages_sent
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
        chat_id: &str,
        text: &str,
    ) -> Result<()> {
        self.state.lock().await.event_log.push(MockEvent::Edited {
            chat_id: chat_id.to_string(),
            text: text.to_string(),
        });
        Ok(())
    }

    async fn send_photo(
        &self,
        _data: &[u8],
        filename: &str,
        _caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        self.state
            .lock()
            .await
            .event_log
            .push(MockEvent::PhotoSent {
                chat_id: chat_id.to_string(),
                filename: filename.to_string(),
            });
        Ok(match self.ptype {
            PlatformType::Telegram => PlatformMessageId::Telegram(1),
            PlatformType::Slack => PlatformMessageId::Slack("1.0".to_string()),
            PlatformType::Discord => PlatformMessageId::Discord(1),
        })
    }

    async fn send_document(
        &self,
        _data: &[u8],
        filename: &str,
        _caption: Option<&str>,
        chat_id: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        self.state
            .lock()
            .await
            .event_log
            .push(MockEvent::DocumentSent {
                chat_id: chat_id.to_string(),
                filename: filename.to_string(),
            });
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
        let mut s = self.state.lock().await;
        s.event_log.push(MockEvent::Paused);
        s.pause_count += 1;
    }

    async fn resume(&self) {
        let mut s = self.state.lock().await;
        s.event_log.push(MockEvent::Resumed);
        s.resume_count += 1;
    }
}
