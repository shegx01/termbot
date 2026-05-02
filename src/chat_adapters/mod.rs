#[cfg(feature = "discord")]
pub mod discord;
#[cfg(feature = "slack")]
pub mod slack;
#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(feature = "discord")]
pub use discord::DiscordAdapter;

use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformType {
    Telegram,
    Slack,
    Discord,
}

/// Serializable chat routing context.
///
/// Carried in queue job files so the retry worker can send status messages
/// to the correct chat across restarts (queue files are self-describing).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatBinding {
    pub platform: PlatformType,
    pub chat_id: String,
    pub thread_ts: Option<String>,
}

impl From<&ReplyContext> for ChatBinding {
    fn from(ctx: &ReplyContext) -> Self {
        Self {
            platform: ctx.platform,
            chat_id: ctx.chat_id.clone(),
            thread_ts: ctx.thread_ts.clone(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum PlatformMessageId {
    Telegram(i32),
    Slack(String), // message ts
    Discord(u64),
}

/// An image or file attachment downloaded to a temp file.
///
/// # SAFETY
/// Drop removes the file at `path` on a best-effort basis. Cloning
/// `Attachment` shares the path; the second Drop sees `ENOENT` and silently
/// swallows it. Do NOT panic in Drop.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub filename: String,
    #[allow(dead_code)]
    pub media_type: String, // e.g. "image/jpeg", "image/png"
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != ErrorKind::NotFound {
                tracing::debug!(
                    path = %self.path.display(),
                    error = %e,
                    "Attachment::drop: failed to remove temp file"
                );
            }
            // ENOENT is expected when a clone has already removed the file;
            // swallow silently.
        }
    }
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
    /// Socket-origin: client-supplied request ID for correlation.
    #[allow(dead_code)]
    pub socket_request_id: Option<String>,
    /// Socket-origin: client name for tracing spans.
    #[allow(dead_code)]
    pub socket_client_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReplyContext {
    pub platform: PlatformType,
    pub chat_id: String,
    pub thread_ts: Option<String>,
    /// If set, route replies to this channel instead of to the chat platform.
    /// Used by the socket adapter for per-request response routing.
    pub socket_reply_tx: Option<mpsc::UnboundedSender<String>>,
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

    /// Pause user-visible message processing. Default no-op; adapters that
    /// support sleep/wake handling override this.
    async fn pause(&self) {}
    /// Resume user-visible message processing. Default no-op.
    async fn resume(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `PlatformType` variant must survive a serde JSON round-trip with
    /// value equality.  This establishes the variant-name wire contract for
    /// queue job files (which embed `ChatBinding.platform` as a serialized JSON
    /// string).  Changing variant names would break existing queue files.
    #[test]
    fn platform_type_roundtrips_json() {
        for variant in [
            PlatformType::Telegram,
            PlatformType::Slack,
            PlatformType::Discord,
        ] {
            let serialized = serde_json::to_string(&variant)
                .unwrap_or_else(|e| panic!("Failed to serialize {:?}: {}", variant, e));
            let deserialized: PlatformType =
                serde_json::from_str(&serialized).unwrap_or_else(|e| {
                    panic!(
                        "Failed to deserialize '{}' back to PlatformType: {}",
                        serialized, e
                    )
                });
            assert_eq!(
                variant, deserialized,
                "PlatformType::{:?} did not round-trip through JSON",
                variant
            );
        }
    }

    #[test]
    fn chat_binding_from_reply_context_copies_fields() {
        let ctx = ReplyContext {
            platform: PlatformType::Telegram,
            chat_id: "12345".to_string(),
            thread_ts: Some("1234567890.123".to_string()),
            socket_reply_tx: None,
        };
        let binding = ChatBinding::from(&ctx);
        assert_eq!(binding.platform, PlatformType::Telegram);
        assert_eq!(binding.chat_id, "12345");
        assert_eq!(binding.thread_ts, Some("1234567890.123".to_string()));
    }

    #[test]
    fn chat_binding_roundtrips_json() {
        let binding = ChatBinding {
            platform: PlatformType::Slack,
            chat_id: "C123ABC".to_string(),
            thread_ts: None,
        };
        let json = serde_json::to_string(&binding).unwrap();
        let restored: ChatBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.chat_id, binding.chat_id);
        assert!(matches!(restored.platform, PlatformType::Slack));
    }

    /// Golden-string assertion guarding the on-disk queue-file wire format.
    ///
    /// `ChatBinding` is persisted inside every `DeliveryJob` under
    /// `<queue_dir>/pending/<run_id>.json`.  Queue files may survive across
    /// terminus restarts and upgrades.  If a future refactor silently adds
    /// `#[serde(rename = "…")]` or reorders fields in a backwards-incompatible
    /// way, existing pending deliveries could fail to deserialize.  The
    /// round-trip test above only proves the type is self-consistent; this
    /// one pins the exact serialized form.
    #[test]
    fn chat_binding_serializes_to_exact_known_json() {
        let binding = ChatBinding {
            platform: PlatformType::Telegram,
            chat_id: "12345".to_string(),
            thread_ts: None,
        };
        let json = serde_json::to_string(&binding).unwrap();
        assert_eq!(
            json, r#"{"platform":"Telegram","chat_id":"12345","thread_ts":null}"#,
            "ChatBinding JSON wire format changed; this breaks persisted queue \
             files. If intentional, coordinate with a queue-file migration."
        );
    }

    // ── Attachment Drop tests (AC-1, AC-2, AC-clone) ─────────────────────────

    fn make_temp_file() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CTR: AtomicUsize = AtomicUsize::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "terminus-test-att-{}-{}.txt",
            std::process::id(),
            n
        ));
        std::fs::write(&path, b"test data").expect("write temp file");
        path
    }

    fn make_attachment(path: std::path::PathBuf) -> Attachment {
        Attachment {
            path,
            filename: "test.txt".to_string(),
            media_type: "text/plain".to_string(),
        }
    }

    /// AC-1: Dropping an Attachment removes the underlying file.
    #[test]
    fn attachment_drop_removes_file() {
        let path = make_temp_file();
        assert!(path.exists(), "temp file should exist before drop");
        {
            let _att = make_attachment(path.clone());
            // Drop happens here at end of block.
        }
        assert!(
            !path.exists(),
            "temp file should be removed after Attachment is dropped"
        );
    }

    /// AC-2: Dropping an Attachment whose file has already been deleted does
    /// not panic (ENOENT is silently swallowed).
    #[test]
    fn attachment_drop_swallows_enoent() {
        let path = make_temp_file();
        std::fs::remove_file(&path).expect("pre-delete the file");
        assert!(
            !path.exists(),
            "file should be gone before constructing Attachment"
        );
        // Constructing and dropping an Attachment for a missing file must not panic.
        let att = make_attachment(path);
        drop(att); // must not panic
    }

    /// AC-clone: Cloning an Attachment and dropping both copies is safe.
    /// The file is deleted exactly once; the second Drop sees ENOENT and swallows it.
    #[test]
    fn attachment_clone_double_drop_is_safe() {
        let path = make_temp_file();
        assert!(path.exists(), "temp file should exist");
        let att1 = make_attachment(path.clone());
        let att2 = att1.clone();
        drop(att1); // first drop removes the file
        assert!(!path.exists(), "file should be gone after first drop");
        drop(att2); // second drop: ENOENT — must not panic
    }
}
