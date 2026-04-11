use super::{Harness, HarnessEvent, HarnessKind};
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;

#[allow(dead_code)]
pub struct CodexHarness;

#[async_trait]
impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn supports_resume(&self) -> bool {
        false // TBD
    }

    async fn run_prompt(
        &self,
        _prompt: &str,
        _cwd: &Path,
        _session_id: Option<&str>,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        todo!("Codex harness not yet implemented")
    }

    fn get_session_id(&self, _session_name: &str) -> Option<String> {
        None
    }

    fn set_session_id(&self, _session_name: &str, _id: String) {}
}
