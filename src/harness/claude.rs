use super::{truncate, Harness, HarnessEvent, HarnessKind};
use crate::platform::Attachment;
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
        attachments: &[Attachment],
        _cwd: &Path,
        session_id: Option<&str>,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(64);

        // Build the enhanced prompt with @ mentions for image attachments
        let full_prompt = build_image_prompt(prompt, attachments);

        let cwd = self.cwd.clone();
        let resume_session = session_id.map(|s| s.to_string());
        let attachment_paths: Vec<PathBuf> = attachments.iter().map(|a| a.path.clone()).collect();

        tokio::spawn(async move {
            // Catch panics so the channel always closes cleanly with an error event
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> = AssertUnwindSafe(
                run_claude_prompt_inner(full_prompt, cwd, resume_session, event_tx.clone()),
            )
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

            // ALWAYS clean up temp files — runs after normal completion, timeout, error, or panic
            for path in &attachment_paths {
                let _ = tokio::fs::remove_file(path).await;
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
    let prompt_start = std::time::SystemTime::now();
    options.permission_mode = Some(PermissionMode::BypassPermissions);
    let cwd_for_scan = cwd.clone();
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
    let mut written_files: Vec<String> = Vec::new();

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Assistant(assistant)) => {
                for block in &assistant.message.content {
                    match block {
                        ContentBlock::ToolUse(tool_use) => {
                            let tool_name = tool_use.name.clone();
                            let input_desc = describe_tool_input(&tool_name, &tool_use.input);

                            // Track file paths from Write/Edit tools for output delivery
                            if tool_name == "Write" || tool_name == "Edit" {
                                if let Some(path) =
                                    tool_use.input.get("file_path").and_then(|p| p.as_str())
                                {
                                    written_files.push(path.to_string());
                                }
                            }

                            // Track output files from Bash commands (e.g. `pandoc -o report.pdf`)
                            if tool_name == "Bash" {
                                if let Some(cmd) =
                                    tool_use.input.get("command").and_then(|c| c.as_str())
                                {
                                    written_files.extend(extract_bash_output_paths(cmd));
                                }
                            }

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
                // SDK parse errors (e.g. unrecognized ContentBlock types like image
                // responses from Read tool on PDFs) are non-fatal — skip and continue.
                let err_str = e.to_string();
                if err_str.contains("parse") || err_str.contains("did not match any variant") {
                    tracing::warn!("Skipping unparseable SDK message: {}", err_str);
                    continue;
                }
                // True I/O errors are fatal
                let _ = event_tx.send(HarnessEvent::Error(err_str)).await;
                break;
            }
        }
    }

    // Send final text response
    let text = response_text.trim().to_string();
    if !text.is_empty() {
        let _ = event_tx.send(HarnessEvent::Text(text)).await;
    }

    // Discover new deliverable files created during this prompt.
    // Scan both CWD and /tmp to catch scripts that write output to temp directories.
    let tmp_dir = PathBuf::from("/tmp");
    let scan_dirs = [cwd_for_scan.as_path(), tmp_dir.as_path()];
    for dir in &scan_dirs {
        let discovered = scan_new_files(dir, prompt_start).await;
        for path in &discovered {
            if !written_files.contains(path) {
                written_files.push(path.clone());
            }
        }
    }

    // Deduplicate by canonical path (Write tool may track relative, scan returns absolute)
    let mut seen = std::collections::HashSet::new();
    written_files.retain(|p| {
        let canonical = std::fs::canonicalize(p)
            .unwrap_or_else(|_| PathBuf::from(p));
        seen.insert(canonical)
    });

    // Always emit deliverable files, even if the stream ended with an error
    emit_output_files(&written_files, &cwd_for_scan, &event_tx).await;

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

/// Extract output file paths from a Bash command string.
/// Looks for common output flags: `-o file`, `--output file`, `> file`
fn extract_bash_output_paths(cmd: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let tokens: Vec<&str> = cmd.split_whitespace().collect();

    for i in 0..tokens.len() {
        // -o file, --output file, --output=file
        if (tokens[i] == "-o" || tokens[i] == "--output") && i + 1 < tokens.len() {
            paths.push(tokens[i + 1].to_string());
        } else if let Some(rest) = tokens[i].strip_prefix("--output=") {
            paths.push(rest.to_string());
        }
        // > file (shell redirect)
        if tokens[i] == ">" && i + 1 < tokens.len() {
            paths.push(tokens[i + 1].to_string());
        }
    }

    paths
}

/// File extensions that should be delivered back to the chat as attachments.
/// Images are delivered via send_photo, documents via send_document.
fn is_deliverable_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        // Images
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp"
        // Documents
        | "pdf" | "csv" | "xlsx" | "docx" | "pptx"
        // Text/data
        | "md" | "txt" | "json" | "yaml" | "yml" | "toml" | "xml" | "html"
    )
}

/// Derive MIME type from file extension for delivery routing.
fn mime_from_ext(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "yaml" | "yml" => "text/yaml",
        "toml" => "text/toml",
        "xml" => "text/xml",
        "html" => "text/html",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Scan a directory (non-recursive) for deliverable files created after `since`.
async fn scan_new_files(dir: &Path, since: std::time::SystemTime) -> Vec<String> {
    let mut found = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return found,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();

        // Check extension is deliverable
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map_or(false, is_deliverable_extension)
        {
            continue;
        }

        // Skip termbot temp files
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("termbot-img-") {
                continue;
            }
        }
        // Check it's a regular file AND was modified after prompt start (async, non-blocking)
        if let Ok(metadata) = tokio::fs::metadata(&path).await {
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
            if modified > since {
                found.push(path.to_string_lossy().to_string());
            }
        }
    }
    found
}

const MAX_OUTPUT_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB (Telegram doc limit)

/// Check written files and emit HarnessEvent::File for deliverable ones.
/// Sensitive filename patterns that must never be delivered to chat.
const SENSITIVE_PATTERNS: &[&str] = &[
    "termbot.toml",
    ".env",
    "credentials",
    "secret",
    "token",
    "password",
    "private_key",
];

async fn emit_output_files(
    written_files: &[String],
    cwd: &Path,
    event_tx: &mpsc::Sender<HarnessEvent>,
) {
    // Resolve CWD once for path containment checks
    let cwd_canonical = match cwd.canonicalize() {
        Ok(c) => c,
        Err(_) => return,
    };

    for file_path_str in written_files {
        let path = Path::new(file_path_str);

        // Path traversal guard: only deliver files under the working directory or /tmp
        let canonical = match tokio::fs::canonicalize(path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let tmp_dir = PathBuf::from("/tmp").canonicalize().unwrap_or_else(|_| PathBuf::from("/tmp"));
        let in_cwd = canonical.starts_with(&cwd_canonical);
        let in_tmp = canonical.starts_with(&tmp_dir);
        if !in_cwd && !in_tmp {
            tracing::warn!("Refusing to deliver file outside CWD/tmp: {}", file_path_str);
            continue;
        }

        // Skip input attachment temp files
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("termbot-img-") {
                continue;
            }
            // Skip sensitive files
            let name_lower = name.to_ascii_lowercase();
            if SENSITIVE_PATTERNS.iter().any(|s| name_lower.contains(s)) {
                tracing::warn!("Skipping sensitive file from delivery: {}", name);
                continue;
            }
        }

        // Check extension against deliverable allowlist
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) if is_deliverable_extension(e) => e.to_string(),
            _ => continue,
        };

        // Check file exists and is within size limit
        let metadata = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > MAX_OUTPUT_FILE_SIZE {
            tracing::warn!(
                "Output file {} exceeds 50 MB limit ({} bytes), skipping delivery",
                file_path_str,
                metadata.len()
            );
            continue;
        }

        // Read file and emit
        match tokio::fs::read(path).await {
            Ok(data) => {
                let filename = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let media_type = mime_from_ext(&ext);
                tracing::info!("Delivering output file: {} ({})", filename, media_type);
                let _ = event_tx
                    .send(HarnessEvent::File {
                        data,
                        media_type,
                        filename,
                    })
                    .await;
            }
            Err(e) => {
                tracing::warn!("Failed to read output file {}: {}", file_path_str, e);
            }
        }
    }
}

/// Build a prompt that includes `@/path/to/image` mentions for each attachment.
/// If the prompt is empty (photo-only message), generates a default prompt.
fn build_image_prompt(prompt: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return prompt.to_string();
    }

    let mentions: Vec<String> = attachments
        .iter()
        .map(|att| format!("@{}", att.path.display()))
        .collect();
    let mentions_str = mentions.join(" ");

    if prompt.trim().is_empty() {
        // Photo-only message — add a default instruction
        format!("What is in this image? {}", mentions_str)
    } else {
        format!("{} {}", prompt, mentions_str)
    }
}
