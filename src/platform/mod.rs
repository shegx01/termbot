pub mod telegram;
pub mod slack;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlatformType {
    Telegram,
    Slack,
}

#[derive(Debug, Clone)]
pub enum PlatformMessageId {
    Telegram(i32),
    Slack(String), // message ts
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub user_id: String,
    pub text: String,
    pub platform: PlatformType,
    pub reply_context: ReplyContext,
}

#[derive(Debug, Clone)]
pub struct ReplyContext {
    pub platform: PlatformType,
    pub chat_id: String,
    pub thread_ts: Option<String>,
}

#[async_trait]
pub trait ChatPlatform: Send + Sync {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()>;
    async fn send_message(&self, text: &str, chat_id: &str, thread_ts: Option<&str>) -> Result<PlatformMessageId>;
    async fn edit_message(&self, msg_id: &PlatformMessageId, chat_id: &str, text: &str) -> Result<()>;
    fn is_connected(&self) -> bool;
    fn platform_type(&self) -> PlatformType;
}
