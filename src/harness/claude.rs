use super::{Harness, HarnessEvent, HarnessKind};
use anyhow::Result;
use async_trait::async_trait;
use claude_agent_sdk_rust::{
    query, types::content::ContentBlock, ClaudeAgentOptions, Message, PermissionMode,
};
use futures_util::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Claude Code SDK harness with multi-turn session support.
pub struct ClaudeHarness {
    sessions: Mutex<HashMap<String, String>>, // session_name -> claude session_id
    cwd: PathBuf,
}

impl ClaudeHarness {
    pub fn new(cwd: String) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            cwd: PathBuf::from(cwd),
        }
    }
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn supports_resume(&self) -> bool {
        true
    }

    async fn run_prompt(
        &self,
        prompt: &str,
        _cwd: &Path,
        session_id: Option<&str>,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(64);

        let prompt_owned = prompt.to_string();
        let cwd = self.cwd.clone();
        let resume_session = session_id.map(|s| s.to_string());

        tokio::spawn(async move {
            // Catch panics so the channel always closes cleanly with an error event
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> = AssertUnwindSafe(run_claude_prompt_inner(
                prompt_owned,
                cwd,
                resume_session,
                event_tx.clone(),
            ))
            .catch_unwind()
            .await;

            match result {
                Ok(()) => {} // events already sent
                Err(panic_info) => {
                    let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                        format!("Claude: internal panic: {}", s)
                    } else if let Some(s) = panic_info.downcast_ref::<String>() {
                        format!("Claude: internal panic: {}", s)
                    } else {
                        "Claude: internal panic (unknown)".to_string()
                    };
                    let _ = event_tx.send(HarnessEvent::Error(msg)).await;
                }
            }
        });

        Ok(event_rx)
    }

    fn get_session_id(&self, session_name: &str) -> Option<String> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_name).cloned()
    }

    fn set_session_id(&self, session_name: &str, session_id: String) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session_name.to_string(), session_id);
    }
}

/// Inner function that runs the Claude prompt and streams events.
/// Separated so it can be wrapped in catch_unwind.
async fn run_claude_prompt_inner(
    prompt: String,
    cwd: PathBuf,
    resume_session: Option<String>,
    event_tx: mpsc::Sender<HarnessEvent>,
) {
    let mut options = ClaudeAgentOptions::default();
    options.permission_mode = Some(PermissionMode::BypassPermissions);
    options.cwd = Some(cwd);
    options.allowed_tools = vec![
        "Read".to_string(),
        "Write".to_string(),
        "Edit".to_string(),
        "Bash".to_string(),
        "Glob".to_string(),
        "Grep".to_string(),
        "Agent".to_string(),
        "WebSearch".to_string(),
        "WebFetch".to_string(),
    ];

    if let Some(sid) = resume_session {
        options.resume = Some(sid);
    }

    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        query(&prompt, Some(options)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = event_tx.send(HarnessEvent::Error(e.to_string())).await;
            return;
        }
        Err(_) => {
            let _ = event_tx
                .send(HarnessEvent::Error(
                    "Claude request timed out (5 min limit)".to_string(),
                ))
                .await;
            return;
        }
    };
    let mut stream = pin!(stream);

    let mut response_text = String::new();
    let mut session_id = String::new();

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Assistant(assistant)) => {
                for block in &assistant.message.content {
                    match block {
                        ContentBlock::ToolUse(tool_use) => {
                            let tool_name = tool_use.name.clone();
                            let input_desc = describe_tool_input(&tool_name, &tool_use.input);
                            let _ = event_tx
                                .send(HarnessEvent::ToolUse {
                                    tool: tool_name,
                                    description: input_desc,
                                })
                                .await;
                        }
                        ContentBlock::Text(text_block) => {
                            response_text.push_str(&text_block.text);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Message::Result(result)) => {
                session_id = result.session_id.clone();
            }
            Ok(_) => {}
            Err(e) => {
                let _ = event_tx.send(HarnessEvent::Error(e.to_string())).await;
                return;
            }
        }
    }

    // Send final text response
    let text = response_text.trim().to_string();
    if !text.is_empty() {
        let _ = event_tx.send(HarnessEvent::Text(text)).await;
    }

    let _ = event_tx
        .send(HarnessEvent::Done {
            session_id: session_id.clone(),
        })
        .await;
}

/// Extract a human-readable description from a tool's input JSON.
fn describe_tool_input(tool: &str, input: &serde_json::Value) -> String {
    // Agent tool: show description field, not the raw JSON blob
    if tool == "Agent" {
        if let Some(desc) = input.get("description").and_then(|d| d.as_str()) {
            return desc.to_string();
        }
        if let Some(prompt) = input.get("prompt").and_then(|p| p.as_str()) {
            return truncate(prompt, 60);
        }
    }
    // Bash: show the command
    if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
        return truncate(cmd, 80);
    }
    // Read/Write/Edit: show file path
    if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
        return path.to_string();
    }
    // Grep/Glob: show the pattern
    if let Some(pattern) = input.get("pattern").and_then(|p| p.as_str()) {
        return truncate(pattern, 80);
    }
    // Glob: show the glob pattern
    if let Some(pattern) = input.get("glob").and_then(|p| p.as_str()) {
        return truncate(pattern, 80);
    }
    // Fallback: truncated JSON
    let s = input.to_string();
    truncate(&s, 100)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", &s[..end])
}
