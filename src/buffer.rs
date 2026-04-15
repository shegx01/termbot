use crate::chat_adapters::ChatBinding;
use crate::tmux::TmuxClient;

/// Payload for a structured output result — schema name, validated value, run ID.
#[derive(Debug, Clone)]
pub struct StructuredOutputPayload {
    pub schema: String,
    pub value: serde_json::Value,
    /// ULID of the run that produced this output.  Not currently read by the
    /// delivery task (the webhook path carries run_id via `DeliveryJob`), but
    /// kept in this payload so future tracing/observability work can surface it
    /// in chat-side rendering spans.
    #[allow(dead_code)]
    pub run_id: String,
}

/// Status kind for webhook delivery events emitted by the retry worker.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `Error` variant is reserved for future per-attempt observability events
pub enum WebhookStatusKind {
    /// Webhook POST succeeded (2xx response).
    Delivered,
    /// Job exceeded `max_retry_age_hours` and was moved to `dead/`.
    Abandoned,
    /// An error occurred during delivery (surfaced for observability).
    Error { msg: String },
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    NewMessage {
        session: String,
        content: String,
    },
    SessionExited {
        session: String,
        code: Option<i32>,
    },
    SessionStarted {
        session: String,
        chat_id: String,
        thread_ts: Option<String>,
    },
    /// Emitted once per active chat when a sleep/wake gap is detected or an
    /// unclean restart is detected.  The delivery task renders a human-readable
    /// banner and sends `DeliveryAck::BannerSent` when done.
    GapBanner {
        chat_id: String,
        platform: crate::chat_adapters::PlatformType,
        paused_at: chrono::DateTime<chrono::Utc>,
        resumed_at: chrono::DateTime<chrono::Utc>,
        gap: std::time::Duration,
        missed_count: u32,
    },
    /// Emitted when a structured output result is ready for chat-side rendering.
    ///
    /// The delivery task handles hybrid rendering (inline code block ≤ 3900 B,
    /// attachment otherwise).  Carries a `ChatBinding` so the delivery task
    /// can route without the session-keyed HashMap lookup.
    StructuredOutputRendered {
        payload: StructuredOutputPayload,
        chat: ChatBinding,
    },
    /// Emitted by the retry worker when a queued job is delivered, abandoned,
    /// or encounters a persistent error.
    ///
    /// Uses `ChatBinding` for routing (bypasses the session-keyed lookup used
    /// for streaming terminal output).
    WebhookStatus {
        schema: String,
        run_id: String,
        status: WebhookStatusKind,
        chat: ChatBinding,
    },
    /// Emitted by the retry worker once per chat after a non-empty drain cycle
    /// completes.  One event per unique `ChatBinding` whose pending jobs were
    /// delivered in this cycle.  Chat message: `✅ queue drained (N delivered)`.
    QueueDrained {
        delivered_count: u32,
        chat: ChatBinding,
    },
}

/// Captures terminal output by diffing tmux pane content between polls.
///
/// Uses anchored suffix-diffing: at snapshot-time, record `snapshot.lines().count()`
/// as `anchor`; on each poll, emit `current.lines().skip(anchor)` after stripping
/// the echoed command line and running surviving lines through `is_noise_line`.
///
/// For Telegram: each command's output is sent as a NEW message (not edited in place).
pub struct OutputBuffer {
    session_name: String,
    /// Raw pane capture taken before the command was sent. Retained
    /// for the fast-path byte-equality check in poll().
    snapshot: String,
    /// Line index into `snapshot.lines()` marking where new output
    /// begins for the currently pending command.
    anchor: usize,
    last_command: Option<String>,
    offline_buffer: String,
    offline_buffer_max_bytes: usize,
    connected: bool,
    waiting_for_output: bool,
}

impl OutputBuffer {
    pub fn new(session_name: &str, offline_buffer_max_bytes: usize) -> Self {
        Self {
            session_name: session_name.to_string(),
            snapshot: String::new(),
            anchor: 0,
            last_command: None,
            offline_buffer: String::new(),
            offline_buffer_max_bytes,
            connected: true,
            waiting_for_output: false,
        }
    }

    #[allow(dead_code)]
    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    #[allow(dead_code)]
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Snapshot the current pane content before sending a command.
    /// After this, `poll()` emits any lines appended beyond `anchor`.
    pub async fn snapshot_before_command(&mut self, tmux: &TmuxClient, command: Option<&str>) {
        if let Ok(capture) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            self.anchor = capture.lines().count();
            self.snapshot = capture;
        }
        self.last_command = command.map(|s| s.to_string());
        self.waiting_for_output = true;
    }

    /// Initialize the snapshot to the current pane content
    /// (call on session creation/reconnect).
    pub async fn sync_offset(&mut self, tmux: &TmuxClient) {
        if let Ok(capture) = tmux.capture_pane_with_scrollback(&self.session_name).await {
            self.anchor = capture.lines().count();
            self.snapshot = capture;
        }
    }

    /// Poll for new output by comparing current pane content against the snapshot.
    /// Returns new, non-noise lines as StreamEvents.
    pub async fn poll(&mut self, tmux: &TmuxClient) -> Vec<StreamEvent> {
        if !self.waiting_for_output {
            return Vec::new();
        }

        let current = match tmux.capture_pane_with_scrollback(&self.session_name).await {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        // Fast path: nothing changed
        if current == self.snapshot {
            return Vec::new();
        }

        tracing::info!(
            "[poll {}] content changed ({} -> {} bytes)",
            self.session_name,
            self.snapshot.len(),
            current.len(),
        );

        self.diff_lines(current)
    }

    /// Filter the new pane capture against the anchor, stripping echo and noise.
    /// Called by `poll()` after the fast-path equality check. Also used directly
    /// by unit tests (via `set_state_for_test`) without a live tmux server.
    fn diff_lines(&mut self, current: String) -> Vec<StreamEvent> {
        // Hard cap on lines scanned per tick. Keeps the `Vec<&str>` sized
        // proportional to this constant rather than to unbounded pane
        // scrollback from commands like `yes` or a giant `cat`. We keep the
        // TAIL when the cap is exceeded because new output is always at the
        // bottom of the pane.
        const MAX_SUFFIX_LINES: usize = 10_000;

        let mut events = Vec::new();

        let current_line_count = current.lines().count();

        // Scrollback eviction or full-screen `clear`: the pane contracted
        // below the anchor. Fall through to normal processing with
        // `anchor = 0` so that any real output appended after the clear
        // (within the same tick) is still picked up. Prompt/noise lines
        // are filtered by the normal pipeline.
        let effective_anchor = if current_line_count < self.anchor {
            tracing::debug!(
                session = %self.session_name,
                anchor = self.anchor,
                current_lines = current_line_count,
                "pane contracted below anchor; resyncing and scanning full pane"
            );
            0
        } else {
            self.anchor
        };

        // Borrow `last_command` without taking ownership. Taking on every
        // call corrupts subsequent ticks when the first post-command tick
        // produced only noise and returned empty (the echo then arrives on
        // a later tick with `cmd = None` and leaks into chat output).
        // `snapshot_before_command` overwrites `last_command` for the next
        // command; we clear it here only after a successful emit.
        let cmd: Option<String> = self.last_command.clone();

        // Collect the full suffix first, then tail-slice if over cap so we
        // still see the most recent output.
        let suffix_full: Vec<&str> = current.lines().skip(effective_anchor).collect();
        let (suffix, truncated_head): (&[&str], usize) = if suffix_full.len() > MAX_SUFFIX_LINES {
            let drop = suffix_full.len() - MAX_SUFFIX_LINES;
            tracing::warn!(
                session = %self.session_name,
                total_suffix = suffix_full.len(),
                kept = MAX_SUFFIX_LINES,
                dropped_head = drop,
                "suffix exceeds cap; keeping tail only"
            );
            (&suffix_full[drop..], drop)
        } else {
            (&suffix_full[..], 0)
        };

        let mut dropped_as_echo: usize = 0;
        let mut dropped_as_noise: usize = 0;
        let mut surviving: Vec<&str> = Vec::with_capacity(suffix.len());
        let mut echo_stripped = false;

        for line in suffix.iter() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            // Strip the first echoed command line, if present and known.
            if !echo_stripped {
                if let Some(cmd_s) = cmd.as_deref() {
                    if is_command_echo(t, cmd_s) {
                        echo_stripped = true;
                        dropped_as_echo += 1;
                        continue;
                    }
                }
            }
            // After echo_stripped is set, pass `None` so `is_noise_line`
            // does not also run its own echo classification — it would use
            // the same cmd and could over-match legitimate output that
            // happens to begin with a prompt prefix on a subsequent line.
            let noise_cmd = if echo_stripped { None } else { cmd.as_deref() };
            if is_noise_line(line, noise_cmd) {
                dropped_as_noise += 1;
                continue;
            }
            surviving.push(line);
        }

        // Strip a trailing path-style prompt line (e.g. `"~/proj $"`) if
        // present. Prompts always appear at the bottom of the suffix, so
        // this last-line-only check avoids false-positives on real output
        // lines anywhere else in the suffix.
        if let Some(last) = surviving.last() {
            if looks_like_path_prompt(last.trim()) {
                surviving.pop();
                dropped_as_noise += 1;
            }
        }

        let content = surviving.join("\n").trim().to_string();

        if content.is_empty() {
            tracing::debug!(
                session = %self.session_name,
                snapshot_lines = self.anchor,
                current_lines = current_line_count,
                dropped_as_echo,
                dropped_as_noise,
                suffix_truncated_head = truncated_head,
                first_suffix_line = %first_trimmed_truncated(suffix, 120),
                "poll produced empty content"
            );
            // IMPORTANT: do NOT advance anchor/snapshot on empty ticks,
            // and do NOT consume `last_command`. See AC-6 and ADR: the old
            // ratchet at :134-135 is the defect we are specifically
            // removing, and consuming the command here would drop echo
            // stripping on the next tick.
            // Special case: on scrollback contraction we still need to
            // keep anchor inside the pane's current line count, otherwise
            // the next tick will keep tripping the eviction branch. Clamp
            // anchor down but leave snapshot pointing at the new `current`
            // so the fast-path equality check still works.
            if current_line_count < self.anchor {
                self.anchor = current_line_count;
                self.snapshot = current;
            }
            return events;
        }

        // Real output emitted — advance anchor and snapshot, and consume
        // `last_command` so stale echoes from this command can't be
        // re-stripped from unrelated future output.
        self.anchor = current_line_count;
        self.snapshot = current;
        self.last_command = None;

        if !self.connected {
            self.offline_buffer.push_str(&content);
            self.offline_buffer.push('\n');
            self.truncate_offline_buffer();
            return events;
        }

        events.push(StreamEvent::NewMessage {
            session: self.session_name.clone(),
            content,
        });

        events
    }

    #[allow(dead_code)]
    pub fn reconnect(&mut self) -> Option<StreamEvent> {
        self.connected = true;
        // Reset anchor/snapshot so the next poll doesn't double-emit the
        // offline-buffered content or diff against a stale pre-disconnect
        // snapshot. The next `snapshot_before_command` or `sync_offset`
        // call will re-establish a fresh anchor.
        self.anchor = 0;
        self.snapshot = String::new();
        if self.offline_buffer.is_empty() {
            return None;
        }
        let content = std::mem::take(&mut self.offline_buffer);
        Some(StreamEvent::NewMessage {
            session: self.session_name.clone(),
            content: format!("[Catch-up]\n{}", content),
        })
    }

    fn truncate_offline_buffer(&mut self) {
        if self.offline_buffer.len() > self.offline_buffer_max_bytes {
            let excess = self.offline_buffer.len() - self.offline_buffer_max_bytes;
            let safe_start = self.offline_buffer[excess..]
                .char_indices()
                .next()
                .map(|(i, _)| excess + i)
                .unwrap_or(self.offline_buffer.len());
            let truncated_msg = format!("[... {} bytes truncated ...]\n", safe_start);
            self.offline_buffer =
                format!("{}{}", truncated_msg, &self.offline_buffer[safe_start..]);
        }
    }
}

/// Prompt prefixes used by `is_command_echo` and `looks_like_path_prompt`.
/// Sharing the list keeps both helpers in sync.
const PROMPT_CHARS: &[char] = &['$', '%', '#', '❯', '>'];
const PROMPT_PREFIXES: &[&str] = &["$ ", "% ", "# ", "❯ ", "> "];

/// Returns true if `t` is the shell's echo of `cmd`. Matches:
/// - the bare command
/// - a known prompt prefix + command (e.g. `"$ pwd"`, `"❯ pwd"`)
/// - any path-like prefix ending in a prompt char + command
///   (e.g. `"~/proj $ pwd"`, `"user@host:/opt #"` + cmd)
///
/// Returns `false` if `cmd` is empty or whitespace-only, which prevents
/// a bare prompt like `"$ "` from being misclassified as a command echo.
fn is_command_echo(t: &str, cmd: &str) -> bool {
    let cmd_t = cmd.trim();
    if cmd_t.is_empty() {
        return false;
    }
    if t == cmd_t {
        return true;
    }
    for prefix in PROMPT_PREFIXES {
        if let Some(after) = t.strip_prefix(prefix) {
            if after.trim() == cmd_t {
                return true;
            }
        }
    }
    // Generalized form: the line ends with " <cmd>" and the chars just
    // before that are a prompt-looking suffix ending in $/%/#/❯/>. This
    // catches shell prompts with arbitrary path prefixes, e.g.
    // "~/proj $ pwd", "user@host:/opt # ls".
    if let Some(rest) = t.strip_suffix(cmd_t) {
        let rest_trimmed = rest.trim_end();
        if rest_trimmed.len() < rest.len() {
            // there was at least one whitespace char between the prompt and cmd
            if let Some(last_ch) = rest_trimmed.chars().next_back() {
                if PROMPT_CHARS.contains(&last_ch) {
                    return true;
                }
            }
        }
    }
    false
}

/// Returns true if `t` looks like a shell prompt line with a path-like
/// prefix followed by `$`/`%`/`#` (optionally with trailing space already
/// trimmed by the caller). Used only against the LAST suffix line in
/// `diff_lines`, so that a real output line containing similar text
/// (e.g. `grep` hit `"/var/log/syslog #"`) mid-suffix is never stripped.
fn looks_like_path_prompt(t: &str) -> bool {
    for sfx in &[" $", " %", " #"] {
        if let Some(prefix) = t.strip_suffix(sfx) {
            if !prefix.is_empty()
                && !prefix.contains(char::is_whitespace)
                && (prefix.starts_with('/') || prefix.starts_with('~') || prefix.contains('/'))
            {
                return true;
            }
        }
    }
    false
}

/// UTF-8-safe truncation + control-char sanitization. Returns the first
/// non-empty trimmed line of `suffix`, with ASCII/Unicode control chars
/// (other than `\t`) stripped, then truncated to at most `cap` Unicode
/// scalar values with an ellipsis if truncation happened.
///
/// Stripping control chars prevents log injection: an adversarial tmux
/// pane could otherwise emit `\r`, `\n`, or ANSI escape sequences that
/// corrupt operator terminals or forge log lines when this value is
/// rendered into `tracing::debug!`. Tmux `capture-pane -p` already strips
/// ANSI by default; this is defense-in-depth.
/// Never byte-slices (no `&t[..cap]`), so never panics on multi-byte UTF-8.
fn first_trimmed_truncated(suffix: &[&str], cap: usize) -> String {
    for l in suffix {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        let cleaned: String = t
            .chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect();
        let char_count = cleaned.chars().count();
        if char_count <= cap {
            return cleaned;
        }
        let truncated: String = cleaned.chars().take(cap).collect();
        return format!("{truncated}…");
    }
    String::new()
}

/// Check if a single line is terminal noise (prompt, echoed command, plugin
/// errors, starship segments). Command-echo classification is delegated to
/// `is_command_echo` so prompt-prefix lists stay in sync in one place.
///
/// NOTE: path-style shell prompts like `"~/proj $"` are NOT classified here.
/// They are stripped only as the LAST suffix line in `diff_lines` via
/// `looks_like_path_prompt`, so a mid-output line containing similar text
/// (e.g. `grep`-hit `"/var/log/syslog #"`) is never silently dropped.
fn is_noise_line(line: &str, command: Option<&str>) -> bool {
    let t = line.trim();

    if t.is_empty() {
        return false;
    }

    // Echoed command line — delegate to is_command_echo (single source of
    // truth for prompt prefixes and the generalized "path + prompt + cmd"
    // form).
    if let Some(cmd) = command {
        if is_command_echo(t, cmd) {
            return true;
        }
    }

    // Bare prompt characters
    if t == "$" || t == "%" || t == "#" {
        return true;
    }

    // Shell plugin errors
    if t.starts_with("(eval):") && t.contains("command not found") {
        return true;
    }

    // Starship prompt: " on " followed by the git-branch Nerd Font glyph.
    // The empty-string alternative previously here was a tautology that matched
    // any line containing " on ". See .omc/specs/deep-dive-lost-ls-pwd-output.md.
    if t.contains(" on ") && t.contains('\u{e0a0}') {
        return true;
    }
    if t.contains("📦 v") || t.contains("🦀 v") || t.contains("☁️") {
        return true;
    }
    if t.contains("FORGE") && t.contains("anthropic.") {
        return true;
    }
    if t.starts_with('󱙺') {
        return true;
    }
    // Prompt-only lines (no spaces = likely bare prompt path)
    if (t.ends_with('$') || t.ends_with('%') || t.ends_with('#')) && !t.contains(' ') {
        return true;
    }

    false
}

#[cfg(test)]
impl OutputBuffer {
    pub(crate) fn set_state_for_test(&mut self, snap: String, last_command: Option<String>) {
        self.anchor = snap.lines().count();
        self.snapshot = snap;
        self.last_command = last_command;
        self.waiting_for_output = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Existing noise-filter tests (preserved, unmodified)
    // -------------------------------------------------------------------------

    #[test]
    fn noise_detection_eval_error() {
        assert!(is_noise_line(
            "(eval):4605: command not found: compdef",
            None
        ));
    }

    #[test]
    fn noise_detection_starship_prompt() {
        assert!(is_noise_line(
            "terminus on  main ()   [?] is 📦 v0.1.0 via 🦀 v1.76.0",
            None
        ));
    }

    #[test]
    fn noise_detection_command_echo() {
        assert!(is_noise_line("$ echo hello", Some("echo hello")));
        assert!(is_noise_line("% echo hello", Some("echo hello")));
        assert!(is_noise_line("echo hello", Some("echo hello")));
    }

    #[test]
    fn real_output_preserved() {
        assert!(!is_noise_line("hello", None));
        assert!(!is_noise_line("total 42", None));
        assert!(!is_noise_line("Hello! How can I help you today?", None));
        assert!(!is_noise_line(
            "drwxr-xr-x  5 user staff  160 Apr 11 src",
            None
        ));
    }

    #[test]
    fn noise_detection_forge_line() {
        assert!(is_noise_line(
            "# claude -p \"hello\"",
            Some("claude -p \"hello\"")
        ));
    }

    #[test]
    fn noise_bare_prompt_chars() {
        assert!(is_noise_line("$", None));
        assert!(is_noise_line("%", None));
        assert!(is_noise_line("#", None));
    }

    #[test]
    fn noise_emoji_prompt() {
        assert!(is_noise_line("something 📦 v0.1.0 via 🦀 v1.76.0", None));
    }

    #[test]
    fn git_output_not_noise() {
        assert!(!is_noise_line("On branch main", None));
        assert!(!is_noise_line("Switched to branch 'main'", None));
        assert!(!is_noise_line("Merge branch 'main' into feature", None));
    }

    #[test]
    fn empty_line_not_noise() {
        assert!(!is_noise_line("", None));
        assert!(!is_noise_line("  ", None));
    }

    // -------------------------------------------------------------------------
    // New tests added by Phase 2 implementation
    // -------------------------------------------------------------------------

    fn make_buf() -> OutputBuffer {
        OutputBuffer::new("term-test", 65536)
    }

    /// AC-1: Fresh session — `: pwd` delivers the command's output to chat.
    #[test]
    fn poll_fresh_session_emits_output() {
        let mut buf = make_buf();
        let snap = "~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("pwd".to_string()));

        // Current pane: snapshot + echo line + real output + new prompt
        let current = format!("{snap}~/proj $ pwd\n/Users/u/proj\n~/proj $ ");
        let events = buf.diff_lines(current);

        assert_eq!(events.len(), 1, "expected one NewMessage event");
        match &events[0] {
            StreamEvent::NewMessage { content, .. } => {
                assert_eq!(content.trim(), "/Users/u/proj");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// AC-2: Repeated identical commands both deliver output (no suppression).
    #[test]
    fn poll_repeated_command_emits_output() {
        let mut buf = make_buf();
        // Snapshot already contains a prior pwd run.
        let snap = "~/proj $ \n~/proj $ pwd\n/Users/u/proj\n~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("pwd".to_string()));

        // Second pwd run appended beyond anchor
        let current = format!("{snap}~/proj $ pwd\n/Users/u/proj\n~/proj $ ");
        let events = buf.diff_lines(current);

        assert_eq!(
            events.len(),
            1,
            "expected one NewMessage for repeated command"
        );
        match &events[0] {
            StreamEvent::NewMessage { content, .. } => {
                assert!(content.contains("/Users/u/proj"), "content: {content}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// AC-3: After restart / reconnect with scrollback, next `: pwd` still delivers.
    #[test]
    fn poll_repeat_in_scrollback_still_emits() {
        let mut buf = make_buf();
        // Simulate a reconnected session with multiple prior pwd runs in scrollback.
        let snap =
            "~/proj $ pwd\n/Users/u/proj\n~/proj $ pwd\n/Users/u/proj\n~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("pwd".to_string()));

        // Fresh pwd run appended beyond anchor
        let current = format!("{snap}~/proj $ pwd\n/Users/u/proj\n~/proj $ ");
        let events = buf.diff_lines(current);

        assert_eq!(events.len(), 1, "expected one NewMessage after reconnect");
        match &events[0] {
            StreamEvent::NewMessage { content, .. } => {
                assert!(content.contains("/Users/u/proj"), "content: {content}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// AC-6: Anchor does NOT advance on an empty-content tick AND
    /// `last_command` is preserved, so echo stripping still works when the
    /// actual output arrives on a later tick. Production code must not
    /// require manual restoration of `last_command` between ticks.
    #[test]
    fn poll_does_not_ratchet_on_empty_tick() {
        let mut buf = make_buf();
        let snap = "~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("pwd".to_string()));

        // First tick: only noise (a prompt line) appended — no real output.
        let tick1 = format!("{snap}~/proj $ ");
        let events1 = buf.diff_lines(tick1);
        assert!(events1.is_empty(), "tick1 should be empty (all noise)");
        // CRITICAL regression guard: last_command must survive an empty tick,
        // otherwise the echo line in tick2 would leak into chat output.
        assert_eq!(
            buf.last_command.as_deref(),
            Some("pwd"),
            "last_command must be preserved across empty ticks"
        );

        // Second tick: real output now present — must still emit, with
        // the echo line still stripped (no manual re-injection required).
        let tick2 = format!("{snap}~/proj $ pwd\n/Users/u/proj\n~/proj $ ");
        let events2 = buf.diff_lines(tick2);

        assert_eq!(
            events2.len(),
            1,
            "tick2 should emit after anchor did not ratchet"
        );
        match &events2[0] {
            StreamEvent::NewMessage { content, .. } => {
                assert_eq!(
                    content.trim(),
                    "/Users/u/proj",
                    "echo line must be stripped on tick2 even though cmd persisted"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        // After a successful emit, last_command is consumed.
        assert!(
            buf.last_command.is_none(),
            "last_command should be cleared after a successful emit"
        );
    }

    /// Scrollback contraction without new output: no events, anchor clamped
    /// to the current line count, no panic.
    #[test]
    fn poll_scrollback_contraction_without_output_is_safe() {
        let mut buf = make_buf();
        let snap = "line1\nline2\nline3\nline4\nline5\n".to_string();
        buf.set_state_for_test(snap, Some("ls".to_string()));

        // Pane contracted: only prior-visible lines remain. Under the new
        // fall-through semantics they are all stripped as "not echo, not
        // matching any noise rule, but also not what we'd call real output
        // after contraction" — they pass through as output unless they
        // match noise rules. For plain "lineN" strings they DO survive as
        // output; verify the contraction case only keeps anchor safe.
        let current = "line3\nline4\nline5\n".to_string();
        let events = buf.diff_lines(current.clone());

        // Under contraction-with-new-lines semantics, "lineN" strings that
        // are not prompts/echoes survive as real output. Assert anchor
        // clamping is safe regardless of whether events were emitted.
        assert!(
            buf.anchor <= current.lines().count(),
            "anchor must not exceed current line count after contraction, got {}",
            buf.anchor
        );
        // If content emitted, it must only contain real output lines.
        if !events.is_empty() {
            if let StreamEvent::NewMessage { content, .. } = &events[0] {
                assert!(
                    content.contains("line3")
                        || content.contains("line4")
                        || content.contains("line5"),
                    "emitted content should be the surviving real lines, got: {content}"
                );
            }
        }
    }

    /// Scrollback contraction WITH new output (e.g. `clear && echo hello`):
    /// under the revised eviction fall-through, the surviving output is
    /// emitted rather than being silently swallowed.
    #[test]
    fn poll_contraction_with_new_output_emits() {
        let mut buf = make_buf();
        // High anchor — snapshot had many lines.
        let snap = "old1\nold2\nold3\nold4\nold5\nold6\nold7\nold8\n~/proj $ \n".to_string();
        buf.set_state_for_test(snap, Some("echo-hello".to_string()));

        // Pane contracted (clear) and new output + prompt appended.
        let current = "~/proj $ echo-hello\nhello\n~/proj $ ".to_string();
        let events = buf.diff_lines(current);

        assert_eq!(
            events.len(),
            1,
            "output after contraction should be emitted, not dropped"
        );
        if let StreamEvent::NewMessage { content, .. } = &events[0] {
            assert_eq!(content.trim(), "hello");
        } else {
            panic!("expected NewMessage");
        }
    }

    /// Mid-suffix line that LOOKS like a path-style prompt (e.g. a grep
    /// hit in `/etc/issue` that happens to end in " #") must NOT be
    /// stripped. Only the trailing prompt line is subject to the
    /// path-prompt rule.
    #[test]
    fn mid_output_path_like_line_is_not_stripped() {
        let mut buf = make_buf();
        let snap = "~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("grep".to_string()));

        // Suffix: echo, two path-prompt-looking lines of REAL output, then
        // the real trailing prompt. Only the trailing prompt should go.
        let current =
            format!("{snap}~/proj $ grep\n/var/log/syslog #\n/etc/nginx/nginx.conf $\n~/proj $ ");
        let events = buf.diff_lines(current);

        assert_eq!(events.len(), 1, "output must be emitted");
        if let StreamEvent::NewMessage { content, .. } = &events[0] {
            assert!(
                content.contains("/var/log/syslog #"),
                "mid-suffix path-like line must survive, got: {content}"
            );
            assert!(
                content.contains("/etc/nginx/nginx.conf $"),
                "mid-suffix path-like line must survive, got: {content}"
            );
            assert!(
                !content.contains("~/proj $"),
                "trailing prompt must be stripped, got: {content}"
            );
        } else {
            panic!("expected NewMessage");
        }
    }

    /// The suffix line cap keeps only the tail when pane output is huge.
    #[test]
    fn huge_suffix_is_capped_to_tail() {
        let mut buf = make_buf();
        let snap = "~/proj $ \n".to_string();
        buf.set_state_for_test(snap.clone(), Some("yes".to_string()));

        // 15_000 "y" lines — well above the 10_000 cap — then a final
        // unique line we can assert survived (tail kept).
        let mut body = String::with_capacity(32_768);
        body.push_str(&snap);
        body.push_str("~/proj $ yes\n");
        for _ in 0..15_000 {
            body.push_str("y\n");
        }
        body.push_str("FINAL-TAIL-MARKER\n");
        body.push_str("~/proj $ ");

        let events = buf.diff_lines(body);
        assert_eq!(events.len(), 1, "huge suffix must still emit");
        if let StreamEvent::NewMessage { content, .. } = &events[0] {
            assert!(
                content.contains("FINAL-TAIL-MARKER"),
                "tail must be kept when cap exceeded"
            );
        } else {
            panic!("expected NewMessage");
        }
    }

    /// AC-5: A line with " on " but no Nerd Font glyph is NOT treated as noise
    /// (pins the fix for the tautology at the old :219).
    #[test]
    fn noise_detection_on_substring_without_glyph_is_not_noise() {
        assert!(
            !is_noise_line("depends on the library", None),
            "line with ' on ' but no git-branch glyph must not be filtered"
        );
    }

    /// Echo line with a unicode prompt prefix is stripped; real output is kept.
    #[test]
    fn command_echo_stripped_with_unicode_prompt() {
        assert!(
            is_command_echo("❯ pwd", "pwd"),
            "unicode prompt prefix should be recognized as echo"
        );
        assert!(
            !is_command_echo("/tmp", "pwd"),
            "real output should not be treated as echo"
        );
    }

    /// `first_trimmed_truncated` is char-boundary-safe on mixed CJK + emoji.
    #[test]
    fn first_trimmed_truncated_utf8_safe() {
        // Build a 130-char string of CJK characters (3 bytes each in UTF-8).
        let long_line: String = "你好世界".chars().cycle().take(130).collect();
        let suffix = vec![long_line.as_str()];
        let result = first_trimmed_truncated(&suffix, 120);
        // Must not panic, and result length (in chars) must be ≤ 121 (120 + ellipsis).
        let char_len = result.chars().count();
        assert!(
            char_len <= 121,
            "truncated result should be ≤ 121 chars, got {char_len}"
        );
        assert!(
            result.ends_with('…'),
            "truncated result should end with ellipsis"
        );
    }

    /// Control characters (ANSI escapes, CR, LF, etc.) must be stripped
    /// from the log field to prevent log injection into operator
    /// terminals that render tracing output with ANSI interpretation.
    #[test]
    fn first_trimmed_truncated_strips_control_chars() {
        let injected = "\u{1b}[2Jhello\r\nFAKE LOG LINE\u{7f}world";
        let suffix = vec![injected];
        let result = first_trimmed_truncated(&suffix, 120);
        assert!(
            !result.contains('\u{1b}'),
            "ESC char must be stripped, got: {result:?}"
        );
        assert!(
            !result.contains('\r') && !result.contains('\n'),
            "CR/LF must be stripped, got: {result:?}"
        );
        assert!(
            !result.contains('\u{7f}'),
            "DEL must be stripped, got: {result:?}"
        );
        assert!(
            result.contains("hello") && result.contains("world"),
            "printable content should survive, got: {result:?}"
        );
    }

    /// `is_command_echo` must not treat an empty or whitespace-only `cmd`
    /// as matching a bare prompt line — that would silently drop all
    /// prompt lines as "echo" even when no command was sent.
    #[test]
    fn is_command_echo_empty_cmd_returns_false() {
        assert!(!is_command_echo("$ ", ""));
        assert!(!is_command_echo("% ", "   "));
        assert!(!is_command_echo("something", ""));
    }

    /// Generalized echo form: `<any path-like prefix> <prompt char> <cmd>`.
    #[test]
    fn is_command_echo_general_prefix_form() {
        assert!(is_command_echo("~/proj $ pwd", "pwd"));
        assert!(is_command_echo("user@host:/opt/app # ls", "ls"));
        assert!(is_command_echo(
            "/very/long/path % git status",
            "git status"
        ));
        // Must NOT match real output lines that merely end with the command.
        assert!(!is_command_echo("tools", "ls"));
        assert!(!is_command_echo("the result is ls", "ls"));
    }

    /// `reconnect()` resets anchor and snapshot so the next poll does not
    /// double-deliver the offline-buffered content or diff against a
    /// stale pre-disconnect snapshot.
    #[test]
    fn reconnect_resets_anchor_and_snapshot() {
        let mut buf = make_buf();
        buf.set_state_for_test("line1\nline2\n".to_string(), Some("ls".to_string()));
        buf.set_connected(false);
        buf.offline_buffer.push_str("catch-up content\n");

        let _ = buf.reconnect();

        assert_eq!(buf.anchor, 0, "reconnect should reset anchor to 0");
        assert_eq!(
            buf.snapshot, "",
            "reconnect should clear snapshot so the next poll diffs fresh"
        );
    }
}
