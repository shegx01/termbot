use super::{truncate, Harness, HarnessEvent, HarnessKind};
use crate::chat_adapters::Attachment;
use crate::command::HarnessOptions;
use crate::structured_output::SchemaRegistry;
use anyhow::Result;
use async_trait::async_trait;
use claude_agent_sdk_rust::{
    query, types::content::ContentBlock, types::options::Effort, types::options::SystemPrompt,
    types::options::SystemPromptPreset, ClaudeAgentOptions, Message, PermissionMode,
};
use futures_util::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Claude Code SDK harness with multi-turn session support.
pub struct ClaudeHarness {
    sessions: Mutex<HashMap<String, String>>, // session_name -> claude session_id
    cwd: PathBuf,
    schema_registry: Arc<SchemaRegistry>,
}

impl ClaudeHarness {
    pub fn new(cwd: String) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            cwd: PathBuf::from(cwd),
            schema_registry: Arc::new(SchemaRegistry::default()),
        }
    }

    /// Set the schema registry for structured output resolution.
    pub fn with_schema_registry(mut self, registry: Arc<SchemaRegistry>) -> Self {
        self.schema_registry = registry;
        self
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
        options: &HarnessOptions,
    ) -> Result<mpsc::Receiver<HarnessEvent>> {
        let (event_tx, event_rx) = mpsc::channel::<HarnessEvent>(64);

        // Build the enhanced prompt with @ mentions for image attachments
        let full_prompt = build_image_prompt(prompt, attachments);

        let cwd = self.cwd.clone();
        let resume_session = session_id.map(|s| s.to_string());
        let attachment_paths: Vec<PathBuf> = attachments.iter().map(|a| a.path.clone()).collect();
        let harness_opts = options.clone();
        let schema_registry = Arc::clone(&self.schema_registry);

        tokio::spawn(async move {
            // Catch panics so the channel always closes cleanly with an error event
            let result: std::result::Result<(), Box<dyn std::any::Any + Send>> =
                AssertUnwindSafe(run_claude_prompt_inner(
                    full_prompt,
                    cwd,
                    resume_session,
                    event_tx.clone(),
                    harness_opts,
                    schema_registry,
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
    harness_opts: HarnessOptions,
    schema_registry: Arc<SchemaRegistry>,
) {
    let mut options = ClaudeAgentOptions::default();
    let prompt_start = std::time::SystemTime::now();
    options.permission_mode = Some(match harness_opts.permission_mode.as_deref() {
        Some("default") => PermissionMode::Default,
        Some("acceptEdits") => PermissionMode::AcceptEdits,
        Some("plan") => PermissionMode::Plan,
        Some("bypassPermissions") | None => PermissionMode::BypassPermissions,
        Some(_) => PermissionMode::BypassPermissions, // validated by parser
    });
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

    // Apply user-supplied harness options
    if let Some(ref model) = harness_opts.model {
        options.model = Some(model.clone());
    }
    if let Some(ref effort_str) = harness_opts.effort {
        options.effort = match effort_str.as_str() {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "max" => Some(Effort::Max),
            _ => None,
        };
    }
    // System prompt handling: --system-prompt replaces, --append-system-prompt
    // appends to the default Claude Code preset. If both are set, --system-prompt
    // takes precedence (append is ignored — can't append to a replaced prompt).
    if let Some(ref sp) = harness_opts.system_prompt {
        options.system_prompt = Some(SystemPrompt::Text(sp.clone()));
    } else if let Some(ref asp) = harness_opts.append_system_prompt {
        // Use the SDK's Preset mechanism so the default Claude Code system prompt
        // is preserved and the user's text is appended to it.
        options.system_prompt = Some(SystemPrompt::Preset(SystemPromptPreset {
            preset_type: "preset".to_string(),
            preset: "claude_code".to_string(),
            append: Some(asp.clone()),
        }));
    }
    if !harness_opts.add_dirs.is_empty() {
        options.add_dirs = harness_opts.add_dirs.clone();
    }
    if let Some(n) = harness_opts.max_turns {
        options.max_turns = Some(n);
    }
    if let Some(ref s) = harness_opts.settings {
        options.settings = Some(s.clone());
    }
    if let Some(ref mcp_path) = harness_opts.mcp_config {
        // Load MCP config from the specified file path via extra_args
        options.extra_args.insert(
            "mcp-config".to_string(),
            Some(mcp_path.to_string_lossy().to_string()),
        );
    }

    // Structured output: --schema flag resolution.
    // Resolved schema name is stored for later use when processing ResultMessage.
    let resolved_schema_name: Option<String> = if let Some(ref schema_name) = harness_opts.schema {
        // Validate schema exists in registry.
        match schema_registry.schema_value(schema_name) {
            Some(schema_value) => {
                // Set output_format on the SDK options.
                options.output_format = Some(serde_json::json!({
                    "type": "json_schema",
                    "schema": schema_value
                }));

                // Clamp max_turns to at least 2 (required for structured output).
                if options.max_turns.map(|n| n < 2).unwrap_or(true) {
                    if options.max_turns.is_some() {
                        // Only warn if explicitly set too low.
                        let _ = event_tx
                            .send(HarnessEvent::Text(
                                "⚠️ --schema requires max_turns ≥ 2; clamping to 2".to_string(),
                            ))
                            .await;
                    }
                    options.max_turns = Some(2);
                }

                Some(schema_name.clone())
            }
            None => {
                // Unknown schema — emit error and return without SDK call.
                let known = schema_registry.schema_names().join(", ");
                let known_display = if known.is_empty() {
                    "(none configured)".to_string()
                } else {
                    known
                };
                // Emit as Text (not Error) so the chat message is exactly
                // `❌ unknown schema 'X' (known: ...)` without the "Error: "
                // wrapper that send_error prepends.
                let _ = event_tx
                    .send(HarnessEvent::Text(format!(
                        "❌ unknown schema '{}' (known: {})",
                        schema_name, known_display
                    )))
                    .await;
                return;
            }
        }
    } else {
        None
    };

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
    let mut last_event_time = tokio::time::Instant::now();
    let mut thinking_sent = false;

    loop {
        // Wait for the next stream message, or fire a thinking indicator on 2s silence
        let msg = tokio::select! {
            msg = stream.next() => msg,
            _ = tokio::time::sleep_until(last_event_time + std::time::Duration::from_secs(2)), if !thinking_sent => {
                // 2s of silence — let the user know the model is still working
                let _ = event_tx
                    .send(HarnessEvent::ToolUse {
                        tool: "Thinking".to_string(),
                        description: String::new(),
                    })
                    .await;
                thinking_sent = true;
                continue;
            }
        };

        let msg = match msg {
            Some(m) => m,
            None => break, // stream ended
        };

        // Reset silence timer on any real event (but keep thinking_sent=true
        // so the indicator only fires once, before the first real content)
        last_event_time = tokio::time::Instant::now();

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

                // Handle structured output from the SDK.
                if let Some(ref schema_name) = resolved_schema_name {
                    match result.subtype.as_str() {
                        "success" => {
                            if let Some(structured_value) = result.structured_output {
                                let run_id = ulid::Ulid::new().to_string();
                                let _ = event_tx
                                    .send(HarnessEvent::StructuredOutput {
                                        schema: schema_name.clone(),
                                        value: structured_value,
                                        run_id,
                                    })
                                    .await;
                            }
                        }
                        "error_max_structured_output_retries" => {
                            let _ = event_tx
                                .send(HarnessEvent::Error(format!(
                                    "Claude failed to produce valid JSON matching schema '{}' \
                                     (max structured output retries exceeded)",
                                    schema_name
                                )))
                                .await;
                        }
                        _ => {}
                    }
                }
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
        let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
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
            .is_some_and(is_deliverable_extension)
        {
            continue;
        }

        // Skip terminus temp files
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("terminus-img-") {
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
    "terminus.toml",
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
        let tmp_dir = PathBuf::from("/tmp")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        let in_cwd = canonical.starts_with(&cwd_canonical);
        let in_tmp = canonical.starts_with(&tmp_dir);
        if !in_cwd && !in_tmp {
            tracing::warn!(
                "Refusing to deliver file outside CWD/tmp: {}",
                file_path_str
            );
            continue;
        }

        // Skip input attachment temp files
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("terminus-img-") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Helper to construct a minimal Attachment for testing build_image_prompt.
    fn make_attachment(path: &str) -> Attachment {
        Attachment {
            path: PathBuf::from(path),
            filename: path.to_string(),
            media_type: "image/png".to_string(),
        }
    }

    // --- build_image_prompt ---

    #[test]
    fn build_image_prompt_no_attachments_returns_prompt_unchanged() {
        assert_eq!(build_image_prompt("hello world", &[]), "hello world");
    }

    #[test]
    fn build_image_prompt_nonempty_prompt_one_attachment_appends_mention() {
        let att = make_attachment("/tmp/photo.png");
        assert_eq!(
            build_image_prompt("describe this", &[att]),
            "describe this @/tmp/photo.png"
        );
    }

    #[test]
    fn build_image_prompt_nonempty_prompt_three_attachments_appends_all_mentions() {
        let atts = vec![
            make_attachment("/tmp/a.png"),
            make_attachment("/tmp/b.png"),
            make_attachment("/tmp/c.png"),
        ];
        assert_eq!(
            build_image_prompt("what are these", &atts),
            "what are these @/tmp/a.png @/tmp/b.png @/tmp/c.png"
        );
    }

    #[test]
    fn build_image_prompt_empty_prompt_one_attachment_uses_default_instruction() {
        let att = make_attachment("/tmp/photo.png");
        assert_eq!(
            build_image_prompt("", &[att]),
            "What is in this image? @/tmp/photo.png"
        );
    }

    #[test]
    fn build_image_prompt_empty_prompt_three_attachments_uses_default_instruction_with_all_mentions(
    ) {
        let atts = vec![
            make_attachment("/tmp/a.png"),
            make_attachment("/tmp/b.png"),
            make_attachment("/tmp/c.png"),
        ];
        assert_eq!(
            build_image_prompt("", &atts),
            "What is in this image? @/tmp/a.png @/tmp/b.png @/tmp/c.png"
        );
    }

    #[test]
    fn build_image_prompt_whitespace_only_prompt_treated_as_empty() {
        let att = make_attachment("/tmp/photo.png");
        assert_eq!(
            build_image_prompt("   ", &[att]),
            "What is in this image? @/tmp/photo.png"
        );
    }

    // --- extract_bash_output_paths ---

    #[test]
    fn extract_bash_output_paths_short_flag_o() {
        assert_eq!(
            extract_bash_output_paths("pandoc -o file.pdf"),
            vec!["file.pdf"]
        );
    }

    #[test]
    fn extract_bash_output_paths_long_flag_output_space() {
        assert_eq!(
            extract_bash_output_paths("cmd --output file.csv"),
            vec!["file.csv"]
        );
    }

    #[test]
    fn extract_bash_output_paths_long_flag_output_equals() {
        assert_eq!(
            extract_bash_output_paths("cmd --output=report.md"),
            vec!["report.md"]
        );
    }

    #[test]
    fn extract_bash_output_paths_shell_redirect() {
        assert_eq!(
            extract_bash_output_paths("echo hello > output.txt"),
            vec!["output.txt"]
        );
    }

    #[test]
    fn extract_bash_output_paths_no_output_flag_returns_empty() {
        assert_eq!(
            extract_bash_output_paths("echo hello"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn extract_bash_output_paths_multiple_flags_collects_all() {
        let paths = extract_bash_output_paths("cmd -o a.pdf > b.txt");
        assert_eq!(paths, vec!["a.pdf", "b.txt"]);
    }

    #[test]
    fn extract_bash_output_paths_o_flag_at_end_returns_empty() {
        assert_eq!(extract_bash_output_paths("cmd -o"), Vec::<String>::new());
    }

    // --- is_deliverable_extension ---

    #[test]
    fn is_deliverable_extension_pdf_is_deliverable() {
        assert!(is_deliverable_extension("pdf"));
    }

    #[test]
    fn is_deliverable_extension_csv_is_deliverable() {
        assert!(is_deliverable_extension("csv"));
    }

    #[test]
    fn is_deliverable_extension_png_is_deliverable() {
        assert!(is_deliverable_extension("png"));
    }

    #[test]
    fn is_deliverable_extension_md_is_deliverable() {
        assert!(is_deliverable_extension("md"));
    }

    #[test]
    fn is_deliverable_extension_json_is_deliverable() {
        assert!(is_deliverable_extension("json"));
    }

    #[test]
    fn is_deliverable_extension_rs_is_not_deliverable() {
        assert!(!is_deliverable_extension("rs"));
    }

    #[test]
    fn is_deliverable_extension_py_is_not_deliverable() {
        assert!(!is_deliverable_extension("py"));
    }

    #[test]
    fn is_deliverable_extension_js_is_not_deliverable() {
        assert!(!is_deliverable_extension("js"));
    }

    #[test]
    fn is_deliverable_extension_exe_is_not_deliverable() {
        assert!(!is_deliverable_extension("exe"));
    }

    #[test]
    fn is_deliverable_extension_uppercase_pdf_is_deliverable() {
        assert!(is_deliverable_extension("PDF"));
    }

    #[test]
    fn is_deliverable_extension_mixed_case_csv_is_deliverable() {
        assert!(is_deliverable_extension("Csv"));
    }

    #[test]
    fn is_deliverable_extension_empty_string_returns_false() {
        assert!(!is_deliverable_extension(""));
    }

    // --- mime_from_ext ---

    #[test]
    fn mime_from_ext_pdf_returns_application_pdf() {
        assert_eq!(mime_from_ext("pdf"), "application/pdf");
    }

    #[test]
    fn mime_from_ext_png_returns_image_png() {
        assert_eq!(mime_from_ext("png"), "image/png");
    }

    #[test]
    fn mime_from_ext_csv_returns_text_csv() {
        assert_eq!(mime_from_ext("csv"), "text/csv");
    }

    #[test]
    fn mime_from_ext_json_returns_application_json() {
        assert_eq!(mime_from_ext("json"), "application/json");
    }

    #[test]
    fn mime_from_ext_unknown_extension_returns_octet_stream() {
        assert_eq!(mime_from_ext("xyz"), "application/octet-stream");
    }

    #[test]
    fn mime_from_ext_uppercase_pdf_returns_application_pdf() {
        assert_eq!(mime_from_ext("PDF"), "application/pdf");
    }

    // --- SENSITIVE_PATTERNS ---

    fn filename_is_sensitive(name: &str) -> bool {
        let name_lower = name.to_ascii_lowercase();
        SENSITIVE_PATTERNS.iter().any(|s| name_lower.contains(s))
    }

    #[test]
    fn sensitive_patterns_terminus_toml_matches() {
        assert!(filename_is_sensitive("terminus.toml"));
    }

    #[test]
    fn sensitive_patterns_env_json_matches() {
        assert!(filename_is_sensitive(".env.json"));
    }

    #[test]
    fn sensitive_patterns_credentials_yaml_matches() {
        assert!(filename_is_sensitive("credentials.yaml"));
    }

    #[test]
    fn sensitive_patterns_report_pdf_does_not_match() {
        assert!(!filename_is_sensitive("report.pdf"));
    }

    #[test]
    fn sensitive_patterns_secret_in_filename_matches() {
        assert!(filename_is_sensitive("my-secret-config.json"));
    }
}
