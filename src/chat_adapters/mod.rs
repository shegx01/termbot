pub mod slack;
pub mod telegram;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlatformType {
    Telegram,
    Slack,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum PlatformMessageId {
    Telegram(i32),
    Slack(String), // message ts
}

/// An image or file attachment downloaded to a temp file.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub filename: String,
    #[allow(dead_code)]
    pub media_type: String, // e.g. "image/jpeg", "image/png"
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    #[allow(dead_code)]
    pub user_id: String,
    pub text: String,
    #[allow(dead_code)]
    pub platform: PlatformType,
    pub reply_context: ReplyContext,
    pub attachments: Vec<Attachment>,
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
    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId>;
    #[allow(dead_code)]
    async fn edit_message(
        &self,
        msg_id: &PlatformMessageId,
        chat_id: &str,
        text: &str,
    ) -> Result<()>;
    async fn send_photo(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId>;
    async fn send_document(
        &self,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId>;
    #[allow(dead_code)]
    fn is_connected(&self) -> bool;
    fn platform_type(&self) -> PlatformType;
}
