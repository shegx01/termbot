use anyhow::Result;
use claude_agent_sdk_rust::{
    query,
    ClaudeAgentOptions,
    Message,
    PermissionMode,
    types::content::ContentBlock,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::pin;
use tokio::sync::{mpsc, Mutex};

/// Events streamed from a Claude session to the chat delivery layer.
#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// Claude is using a tool (Read, Write, Edit, Bash, etc.)
    ToolUse { tool: String, description: String },
    /// Text from Claude's response
    Text(String),
    /// Claude session completed
    Done { session_id: String },
    /// An error occurred
    Error(String),
}

/// Manages Claude Code sessions with real-time streaming.
pub struct ClaudeManager {
    sessions: Mutex<HashMap<String, String>>, // session_name -> claude session_id
    pub cwd: PathBuf,
}

impl ClaudeManager {
    pub fn new(cwd: String) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            cwd: PathBuf::from(cwd),
        }
    }

    /// Get the existing session ID for resume, if any.
    pub async fn get_session_id(&self, session_name: &str) -> Option<String> {
        let sessions = self.sessions.lock().await;
        sessions.get(session_name).cloned()
    }

    /// Store a session ID after a prompt completes.
    pub async fn set_session_id(&self, session_name: &str, session_id: String) {
        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_name.to_string(), session_id);
    }

    /// Clear a session
    pub async fn clear_session(&self, session_name: &str) {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(session_name);
    }
}

/// Run a Claude prompt in a background task, streaming events to `event_tx`.
/// This is a free function (not a method) so it can be spawned in tokio::spawn
/// without lifetime issues.
pub async fn run_claude_prompt(
    prompt: String,
    cwd: PathBuf,
    resume_session: Option<String>,
    event_tx: mpsc::Sender<ClaudeEvent>,
) -> Option<String> {
    let mut options = ClaudeAgentOptions::default();
    options.permission_mode = Some(PermissionMode::AcceptEdits);
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
            let _ = event_tx.send(ClaudeEvent::Error(e.to_string())).await;
            return None;
        }
        Err(_) => {
            let _ = event_tx
                .send(ClaudeEvent::Error("Claude request timed out (5 min limit)".to_string()))
                .await;
            return None;
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
                                .send(ClaudeEvent::ToolUse {
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
                let _ = event_tx.send(ClaudeEvent::Error(e.to_string())).await;
                return None;
            }
        }
    }

    // Send final text response
    let text = response_text.trim().to_string();
    if !text.is_empty() {
        let _ = event_tx.send(ClaudeEvent::Text(text)).await;
    }

    let _ = event_tx
        .send(ClaudeEvent::Done {
            session_id: session_id.clone(),
        })
        .await;

    if session_id.is_empty() {
        None
    } else {
        Some(session_id)
    }
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

/// Format a tool use event for display in chat.
pub fn format_tool_event(tool: &str, description: &str) -> String {
    let icon = match tool {
        "Read" => "📖",
        "Write" => "📝",
        "Edit" => "✏️",
        "Bash" => "💻",
        "Glob" => "🔍",
        "Grep" => "🔎",
        "Agent" => "🤖",
        "WebSearch" | "WebFetch" => "🌐",
        _ => "🔧",
    };

    if description.is_empty() {
        format!("{} {}", icon, tool)
    } else {
        format!("{} {} {}", icon, tool, truncate(description, 80))
    }
}
