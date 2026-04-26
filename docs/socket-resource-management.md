# Socket Subsystem Resource Management

The terminus socket subsystem manages four distinct resource categories: temporary attachment files in `/tmp`, in-memory subscription-store entries, background task lifecycles, and active-connection counter slots. All four are governed by a single unifying design principle: **every spawned resource owns its cleanup** via RAII (`Drop`, `ConnectionGuard`) and cooperative cancellation (`CancellationToken`, `JoinSet`).

Why this matters: terminus is a single-user, multi-day-uptime bot. Even small per-event leaks accumulate over time. A forgotten `tokio::fs::remove_file` for attachment files causes disk pressure in `/tmp` over weeks of uptime. Unbounded subscription-store growth from rotating client names (e.g., `agent-{uuid}` per execution) is a realistic accumulation vector. Background tasks that survive shutdown consume FDs and kernel resources. A panic in a connection task that doesn't decrement the counter permanently shrinks the connection cap. This document traces the cleanup mechanisms and invariants that prevent all four.

## Functionality Matrix

| Leak vector | Failure mode | Cleanup mechanism | Test coverage |
|---|---|---|---|
| **H1: Attachment files** | Parse error in `: ` dispatch | `Attachment::Drop` calls `std::fs::remove_file` | `attachment_cleaned_on_parse_error` |
| **H1: Attachment files** | `HarnessOn` guard rejection (no foreground session, not-resumable, resume-name miss) | Drop on `msg.attachments` at function return | `attachment_cleaned_on_harnesson_guard_*` |
| **H1: Attachment files** | Gemini synchronous reject (attachment unsupported) | Drop on caller-held `Vec<Attachment>` | `attachment_cleaned_on_gemini_reject` |
| **H1: Attachment files** | `run_prompt` returns `Err(...)` | Drop on caller-held vec | `attachment_cleaned_on_run_prompt_err` |
| **H1: Attachment files** | Happy path (claude/opencode harness success) | Drop at `dispatch_one_message` scope-end; defense-in-depth harness-side cleanup at `claude.rs:105-107` and `opencode.rs:221-223` | `attachment_cleaned_on_happy_path` |
| **H2: Subscription store** | Rotating client_names or programmatic agent loops | LRU eviction; oldest entry dropped when cap exceeded on next `save` | `subscription_store_lru_evicts_oldest`, `subscription_store_access_bumps_recency` |
| **H2: Subscription store** | Client removed from `[[socket.client]]` config | Post-reload diff; removed client names are purged via `remove(client_name)` | `config_removal_purges_subscription_store` |
| **H3: Config-watcher task** | SIGINT during file-event burst or normal shutdown | `CancellationToken` in watcher's `tokio::select!` + explicit `drop(watcher)` to release kqueue/inotify FD | `watcher_cancels_within_500ms` |
| **H3: Connection tasks** | Shutdown drain timeout or graceful termination | `JoinSet::join_next` awaits pending tasks; `abort_all()` on `shutdown_drain_secs` timeout | `connection_joinset_drains_on_shutdown` |
| **H4: Connection counter** | Panic inside connection task | `ConnectionGuard::Drop` decrements counter regardless of panic | `connection_guard_panic_safe` |

## Design Invariants

- An `Attachment` owns its file's lifetime; constructing one without expecting scope-end deletion is a bug. See `src/chat_adapters/mod.rs::Attachment` Drop impl (lines 66–80).
- `Attachment::Drop` swallows `ErrorKind::NotFound` (clones share paths; second delete is expected). This is load-bearing: documented at the struct definition with a `// SAFETY:` comment.
- The harness layer (claude, opencode) deletes attachment files via `tokio::fs::remove_file` on the happy path *before* the caller's Drop fires — required so child processes can read `-f <path>`. Removing this defense-in-depth would race the child read. Both cleanup layers are required: Drop is the safety net for paths that never reach a harness; harness-side cleanup is deterministic on the happy path.
- `SubscriptionStoreInner` maintains the invariant: `map.contains_key(k) ↔ order.contains(&k)` at all times; both must update together (see `src/socket/mod.rs::SubscriptionStoreInner`).
- LRU eviction implies: "evicted client reconnecting with `restore_subscriptions: true` gets a fresh state — equivalent to first-time connect". The subscription window is configurable via `[socket] max_subscription_store_entries` (default 100).
- `socket_cancel` must reach every spawned task: connection JoinSet members and the config-watcher task. Ordered shutdown in `main.rs` enforces: `socket_cancel.cancel()` → connection JoinSet drain (`shutdown_drain_secs`) → watcher JoinHandle (`shutdown_drain_secs`) → `mark_clean_shutdown`.

## Configuration

The `[socket]` table in `terminus.toml` includes a new field:

```toml
[socket]
enabled = true
bind = "127.0.0.1"
port = 7645
max_subscription_store_entries = 100  # Hard cap on distinct clients whose subscriptions are saved for reconnect-restore
```

**Explanation**: `max_subscription_store_entries` bounds the in-memory subscription map. When a new client's subscriptions exceed capacity, the least-recently-used entry is evicted. Evicted clients will restore no subscriptions on the next `hello` — equivalent to a fresh connect. Default 100 is ample for single-user; configurable for agent-loop scenarios with many transient clients.

## Observability

- **Mutex poisoning recovery** (promoted to this PR from a follow-up): Each of 8 `.expect("...mutex poisoned")` sites in `src/socket/connection.rs` (lines 97, 206, 268, 283, 396, 422, 531, 794) is replaced with `unwrap_or_else(|poisoned| { tracing::warn!(target: "socket", "recovered from poisoned mutex"); poisoned.into_inner() })`. A `tracing::warn!` fires when a mutex poisoning is recovered; this should never occur under normal operation, but recovery allows the connection task to continue serving subsequent frames. If observed in production logs, investigate the original panic.
- **Attachment cleanup failures** (non-ENOENT): `Attachment::Drop` emits `tracing::debug!` only when `remove_file` fails for reasons other than `NotFound`. This is purely defensive; under normal operation, files should delete cleanly on scope-end.
- **Power-management assertions**: Verify idle-sleep prevention via `pmset -g assertions` (macOS) or `systemd-inhibit --list` (Linux). The socket subsystem does not expose its own metrics in v1.

## Verification

To manually verify the leak fixes have landed correctly:

### Attachment cleanup probe (AC-24 style)

```bash
# Terminal 1: start terminus with socket enabled
cargo run

# Terminal 2: send a binary attachment + malformed command via socket
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer <token>" <<'EOF'
{"type": "attachment_meta", "request_id": "bad-1", "filename": "test.txt", "content_type": "text/plain", "size_bytes": 100}
EOF

# (send a binary frame, then a malformed command)
# Then check:
ls /tmp/terminus-attachment-* 2>/dev/null   # should be empty
```

### Watcher cleanup probe (AC-25 style, macOS)

```bash
# Terminal 1: start terminus
cargo run

# Terminal 2: capture the process ID, then send SIGINT
ps aux | grep terminus
lsof -p <pid> | grep -E 'kqueue|inotify|terminus\.toml'  # note the watch on terminus.toml

# Then SIGINT the running process in Terminal 1
# After the "terminus stopped" log line, re-run:
lsof -p <pid> 2>/dev/null | wc -l   # should be 0 (process exited)
```

### Counter panic-safety test

```bash
cargo test --lib socket::ConnectionGuard connection_guard_panic_safe
```

Exercises the panic-safe counter by spawning a task that panics inside a `ConnectionGuard` scope and asserts the counter recovers its initial value.

## Known Limitations

- **Codex harness stub**: The Codex harness currently rejects attachments unconditionally. When it gains attachment support, the `Attachment::Drop` pattern automatically extends coverage to the reject path — no harness-side cleanup code is required for safety.
- **Linux FD-counting test**: A Linux-specific test counting `/proc/self/fd` entries before/after watcher cancel is deferred to a follow-up issue. AC-27 verifies `JoinHandle::is_finished()` instead, which is platform-agnostic and covers both macOS and Linux CI.
- **Lock-poisoning observability**: The `tracing::warn!` events on poisoning recovery could be promoted to a metric/counter if they ever appear in production logs; this is deferred.
