# Slack Feature Parity

This document describes the wake-recovery, attachment, smart-quote, and
thread-edit improvements that bring the Slack adapter to parity with the
Telegram adapter.

## Functionality Matrix

| Capability | Telegram | Slack | Discord | Notes |
|---|---|---|---|---|
| Lossless wake-recovery | Yes (offset) | Yes (watermark + catchup) | No (events discarded) | Slack: `conversations.history` fetch on wake |
| Pause/resume on wake | Yes (watch channel) | Yes (watch channel) | Yes (handler gate) | All level-triggered |
| Smart-quote normalization | Yes | Yes | Yes | `tmux::normalize_quotes` |
| Non-image file forwarding | Yes | Yes | No (text only) | Slack widened from image-only |
| Thread-edit context preserved | Yes | Yes | N/A | `thread_ts_map` ring (cap 50) |
| Watermark persistence | Yes (offset) | Yes (`slack_watermarks`) | No | Force-persisted, bypasses debounce |
| Persistence-failure visibility | Yes | Yes | Yes | Chat-safe warning on send error |
| Per-channel dedup | N/A | Yes (`dedup_window` ring, cap 200) | N/A | Prevents double-delivery on reconnect |

## How to Verify Lossless Recovery

End-to-end smoke test (requires a running terminus instance with Slack configured):

1. **Close the lid** (or suspend the machine) for at least 60 seconds while
   terminus is running.
2. **Send 5 distinct messages** from your phone to the configured Slack channel
   during the sleep window.
3. **Open the lid** (or resume). Wait for terminus to wake.
4. **Verify** in your Slack channel that:
   (a) A gap banner arrives **first** (`⏸ paused at HH:MM, resumed at HH:MM (gap: Xm Ys)`).
   (b) All 5 messages arrive **in send-order** (oldest first) after the banner.
   (c) No message appears **twice** (dedup_window).
5. **Inspect `terminus-state.json`** to confirm `slack_watermarks` contains the
   channel ID → latest ts mapping.

## Architecture Notes

### Socket Mode + History Catchup

The Slack adapter uses two complementary transport mechanisms:

- **Socket Mode WebSocket** — steady-state real-time delivery. Outbound events
  are acknowledged per-envelope. The WebSocket reconnects automatically with
  exponential backoff (5s initial, 300s max). Messages sent while the connection
  is down are **not replayed** by Slack — this is the gap that catchup covers.

- **`conversations.history` catchup** — recovery mechanism triggered by
  `PowerSignal::GapDetected` in `App::handle_gap`. Fetches messages newer than
  `paused_at` (expressed as a Slack-format ts `"{unix_secs}.{micros:06}"`),
  paginated via `response_metadata.next_cursor`.

### `last_seen_ts` Watermark

- Per-channel `HashMap<String, String>` inside `SlackPlatform`.
- Advanced by `parse_event` (Socket Mode push) and `run_catchup` — only
  forward (monotonic per Slack's ts ordering).
- Persisted via `StateUpdate::SlackWatermark { channel_id, ts }` to
  `terminus-state.json` under `slack_watermarks`.
- Force-persisted (bypasses the 10-update / 5s debounce) because it is
  safety-critical for lossless recovery.
- Seeded on startup from `store.snapshot().slack_watermarks` via
  `SlackPlatform::seed_watermarks`.

### Dedup Boundary

The `dedup_window` is a `VecDeque<(channel_id, ts)>` capped at 200 entries.
Oldest entries are dropped on overflow. A message is skipped in `run_catchup`
if its `(channel, ts)` pair is already in the window. `parse_event` inserts
every successfully processed Socket Mode message.

### Pagination Cursor Loop

`run_catchup` follows `response_metadata.next_cursor` until it is absent or
empty. Each page fetches up to 100 messages. Messages are returned
newest-first by Slack and reversed to emit oldest-first on `cmd_tx`.

### Persistence-Failure Visibility Path

If `state_tx.send(StateUpdate::SlackWatermark { ... })` returns `Err` (state
worker channel closed or full), `run_catchup` posts the following text
**directly to the channel via `chat.postMessage`**:

> Slack catchup watermark persistence failed; this prompt's recovery may not
> survive a restart

This is posted directly rather than routed through `cmd_tx` (which goes through
`handle_command`) to prevent the warning text from being injected as stdin to
the foreground tmux session. It follows the same visibility principle as
`App::persist_named_session`.

**Important:** This warning is emitted ONLY when state-store persistence fails
(state worker channel closed or full). It is NOT emitted for Slack API errors
such as `missing_scope`, `not_in_channel`, or `token_revoked`. Those errors are
logged at `WARN` level with channel context, the channel is silently skipped
for that catchup attempt, and the watermark is NOT advanced.

To detect API failures in production, run terminus at `WARN` level or higher
and grep for `"Slack catchup"` or `"Slack conversations.history"` in the logs.

## Setup Verification

Before deploying, ensure your Slack bot token has the following OAuth scopes:

| Scope | Required for |
|---|---|
| `channels:history` | Public channel history catchup |
| `groups:history` | Private channel history catchup |
| `im:history` | Direct message history catchup |
| `chat:write` | Sending messages |
| `files:write` | Uploading files |
| `connections:write` | Opening Socket Mode connections |

Missing `channels:history`, `groups:history`, or `im:history` causes catchup to
fail with a Slack API error (e.g. `not_in_channel` or `missing_scope`). The
error is logged at `WARN` level, the affected channel is skipped for that
catchup attempt, and the watermark is NOT advanced. No chat warning is shown
to the user — API errors do not trigger the persistence-failure warning path.
Operators should monitor logs for `"Slack catchup API error"` messages.

## Recovery

Common failure scenarios and how to recover:

- **Corrupted or stale `slack_watermarks`:** Delete the `slack_watermarks` field
  from `terminus-state.json` and restart. Catchup will re-fetch from the
  `paused_at` timestamp on the next wake.

- **Bot kicked from channel:** Re-invite the bot. No restart required — the
  next wake will trigger catchup and the channel will be fetched again.

- **Token revoked:** Re-authorize the Slack app via the Slack App configuration
  portal, update `bot_token` in `terminus.toml`, and restart. Watermarks are
  preserved; catchup picks up where it left off.

- **State worker stalled (persistence-failure warning visible in chat):**
  This indicates the state channel is full or closed. Restart terminus. The
  in-memory watermark was still advanced, so only messages delivered after the
  warning may need manual review.

## Known Limitations

1. **90-day workspace retention bound (free plans):** `conversations.history`
   returns no messages older than 90 days on free Slack workspaces. Wakes
   longer than 90 days will appear gapless from Slack's perspective, even though
   messages were sent.

2. **Mid-sleep new-DM binding:** If a user sends a DM from a new conversation
   (never previously seen by terminus) during the sleep window, terminus
   cannot discover that channel ID from `conversations.history` alone. The
   first message from the user on wake creates the channel binding. This
   matches Telegram's behavior where new group joins during sleep require
   a first-message-on-resume to bind.

3. **Socket Mode WebSocket buffer-window:** There is a brief overlap window
   between the Socket Mode reconnect (`~2s`) and the `run_catchup` call
   (triggered after banner ack, `~5s`). The `dedup_window` ring prevents
   double-delivery for messages that arrive via both paths.

4. **`conversations.history` rate limit (tier 3, ~50 req/min):** On 429,
   terminus sleeps `Retry-After` seconds and retries once. Total per-channel
   retry time is capped at 30 seconds. If the retry also fails, the channel is
   skipped and the pre-catchup watermark is preserved for the next wake.

5. **Mid-catchup HTTP error preserves watermark:** If the HTTP call fails on
   any page (including paginated fetches), terminus does **not** advance the
   `last_seen_ts` watermark for that channel. The next wake will retry from the
   same starting point, which may result in duplicate messages being re-delivered
   for any pages that did complete before the error.
