use super::{Harness, HarnessEvent, HarnessKind};
use crate::chat_adapters::Attachment;
use crate::command::HarnessOptions;
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;

#[allow(dead_code)]
pub struct GeminiHarness;

#[async_trait]
impl Harness for GeminiHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Gemini
    }

    fn supports_resume(&self) -> bool {
        false // TBD
    }

    async fn run_prompt(
        &self,
        _prompt: &str,
        _attachments: &[Attachment],
        _cwd: &Path,
        _session_id: Option<&str>,
        _options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        Err(anyhow::anyhow!("Gemini harness not yet implemented"))
    }

    fn get_session_id(&self, _session_name: &str) -> Option<String> {
        None
    }

    fn set_session_id(&self, _session_name: &str, _id: String) {}
}
