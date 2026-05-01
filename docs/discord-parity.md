# Discord Feature Parity

This document describes the wake-recovery, attachment, smart-quote, and
thread-aware improvements that bring the Discord adapter to parity with the
Slack and Telegram adapters.

## Functionality Matrix

| Capability | Telegram | Slack | Discord | Notes |
|---|---|---|---|---|
| Lossless wake-recovery | Yes (offset) | Yes (watermark + catchup) | Yes (snowflake + catchup) | Discord: REST `messages` fetch on wake |
| Pause/resume on wake | Yes (watch channel) | Yes (watch channel) | Yes (handler gate) | All level-triggered |
| Smart-quote normalization | Yes | Yes | Yes | `tmux::normalize_quotes` |
| Non-image file forwarding | Yes | Yes | Yes | All accept all MIME types |
| Thread-edit context preserved | Yes | Yes | Yes | Discord: hybrid Threads (guilds) + MessageReference (DMs) |
| Watermark persistence | Yes (offset) | Yes (`slack_watermarks`) | Yes (`discord_watermarks`) | Force-persisted, bypasses debounce |
| Persistence-failure visibility | Yes | Yes | Yes | Chat-safe warning on send error |
| Per-channel dedup | N/A | Yes (`dedup_window` ring, cap 200) | Yes (`dedup_window` ring, cap 200) | Prevents double-delivery on reconnect |

## How to Verify Lossless Recovery

End-to-end smoke test (requires a running terminus instance with Discord configured):

1. **Close the lid** (or suspend the machine) for at least 60 seconds while
   terminus is running.
2. **Send 5 distinct messages** from your Discord account to the configured channel
   during the sleep window.
3. **Open the lid** (or resume). Wait for terminus to wake.
4. **Verify** in your Discord channel that:
   (a) A gap banner arrives **first** (`⏸ paused at HH:MM, resumed at HH:MM (gap: Xm Ys)`).
   (b) All 5 messages arrive **in send-order** (oldest first) after the banner.
   (c) No message appears **twice** (dedup_window).
5. **Inspect `terminus-state.json`** to confirm `discord_watermarks` contains the
   channel ID → latest snowflake mapping.

## Architecture Notes

### Gateway + REST History Catchup

The Discord adapter uses two complementary mechanisms:

- **Gateway WebSocket** — steady-state real-time delivery via serenity's handler model.
  Messages sent while the connection is down are **not replayed** by Discord — this is
  the gap that catchup covers.

- **REST `messages` catchup** — recovery mechanism triggered by `PowerSignal::GapDetected`
  in `App::handle_gap`. Fetches messages newer than `last_seen_message_id` (expressed
  as a Discord snowflake `u64`), paginated via `GetMessages::new().after(...).limit(100)`.

### `last_seen_message_id` Watermark

- Per-channel `HashMap<String, u64>` inside `DiscordAdapter`.
- Advanced by gateway `message` event handler and `run_catchup` — only forward
  (monotonic per snowflake ordering).
- Persisted via `StateUpdate::DiscordWatermark { channel_id, message_id }` to
  `terminus-state.json` under `discord_watermarks`.
- Force-persisted (bypasses the 10-update / 5s debounce) because it is
  safety-critical for lossless recovery.
- Seeded on startup from `store.snapshot().discord_watermarks` via lazy initialization
  in the first gateway message or catchup call.

### Dedup Boundary

The `dedup_window` is a `VecDeque<u64>` (Discord message IDs) capped at 200 entries.
Oldest entries are dropped on overflow. A message is skipped in `run_catchup` if its
snowflake is already in the window. The gateway message handler inserts every
successfully processed message.

### Pagination Loop

`run_catchup` follows `GetMessages` pagination until no messages are returned (or a
hard cap of 1000 messages per channel to bound replay storms; once cap hit, log WARN
and resume normal handling). Messages are returned in descending order by Discord and
reversed to emit oldest-first on `cmd_tx`.

### Persistence-Failure Visibility Path

If `state_tx.send(StateUpdate::DiscordWatermark { ... })` returns `Err` (state worker
channel closed or full), `run_catchup` posts the following text **directly to the
channel via Discord's REST API**:

> Discord catchup watermark persistence failed; this prompt's recovery may not
> survive a restart

This is posted directly rather than routed through `cmd_tx` (which goes through
`handle_command`) to prevent the warning text from being injected as stdin to the
foreground tmux session. It follows the same visibility principle as
`App::persist_named_session`.

**Important:** This warning is emitted ONLY when state-store persistence fails
(state worker channel closed or full). It is NOT emitted for Discord API errors
such as permission denials (`403`), channel-not-found (`404`), or rate limits.
Those errors are logged at `WARN` level with channel context, the channel is
silently skipped for that catchup attempt, and the watermark is NOT advanced.

To detect API failures in production, run terminus at `WARN` level or higher
and grep for `"Discord catchup"` in the logs.

## Hybrid Thread Primitive

Discord has two thread primitives:

- **Threads (CHANNEL_THREAD)** — available only in guild (server) channels. A thread
  is created via `channel_id.create_thread_from_message()` with the user's first
  message as the parent. Subsequent responses are posted directly to the Thread channel,
  grouping them visually and suppressing @mentions to the full guild. Threads have an
  auto-archival TTL (1h, 24h, 3d, or 7d depending on server defaults); archived threads
  can still be read but new messages cannot be posted without unarchiving them.

- **MessageReference** — available in all contexts (DMs and guild channels). A
  message can reply to another message via `CreateMessage::reference_message()`,
  visually threading it in the UI without creating a separate channel. In DMs, this
  is the only threading primitive.

**Hybrid delivery strategy:**

The outbound delivery primitive (Thread vs MessageReference) is selected by
adapter config alone — `guild_id` AND `channel_id` both set → guild-mode →
Thread; otherwise → MessageReference (DM mode). The presence of `guild_id` on
an inbound message is NOT used for this decision.

- **Guild mode** (adapter config has both `guild_id` and `channel_id` set): the
  first outbound response creates a Thread via `create_thread_from_message()`.
  Subsequent responses and edits target the Thread. Per-channel thread mapping
  (`chat_id` → Thread channel_id) is stored in `thread_map` on the adapter.

- **DM mode** (adapter config has no `guild_id` or no `channel_id`): every
  outbound response uses `MessageReference` to the user's original message.

### Thread Auto-Archival Fallback

If a Thread send fails with HTTP 400/403/404 (archived, missing permissions, deleted),
the adapter evicts the `thread_map` entry for that channel, logs a warning, and retries
the message via `MessageReference` in the original channel. This handles Thread
auto-archival gracefully — once archived, subsequent messages fall back to the DM
pattern without user intervention.

## Required Scopes and Intents

**OAuth scopes** (same as existing Discord config; no new grants needed):

| Scope | Required for |
|---|---|
| `View Channels` | Reading messages and thread metadata |
| `Send Messages` | Posting replies and thread creation |
| `Attach Files` | Uploading attachment content |
| `Read Message History` | REST catchup via `GET /channels/{id}/messages` |

**Gateway intents** (privileged; already requested):

| Intent | Required for |
|---|---|
| `MESSAGE_CONTENT` | Reading message text (required for command parsing) |
| `GUILD_MESSAGES` | Receiving message events from guild channels |
| `DIRECT_MESSAGES` | Receiving message events from DMs |

No new scopes or intents are required beyond the existing configuration.

## SSRF Defense

Attachment downloads are restricted to Discord's official CDNs:

- `https://cdn.discordapp.com/` — primary attachment CDN
- `https://media.discordapp.net/` — secondary media CDN

Downloads from other hosts are rejected with a chat-safe error. Mirrors
`slack.rs::is_valid_slack_download_url` at `src/chat_adapters/slack.rs:75-81`.

## Known Limitations

1. **Thread auto-archival evicts `thread_map`:** When a Thread is archived by Discord,
   the next message send to that Thread fails with HTTP 403. The adapter responds by
   evicting the Thread ID from `thread_map` and retrying via `MessageReference` in the
   original channel. The user experiences transparent fallback; no manual action is
   required. The thread_map is in-memory only and is NOT persisted to `terminus-state.json`
   (Threads are session-scoped; on adapter restart, the next message creates a new Thread).

2. **~1000 message cap per channel on catchup:** To prevent runaway replay on extended
   sleeps, `run_catchup` limits pagination to 1000 messages per channel. If more than
   1000 messages were sent during sleep, the oldest messages are not replayed; a WARN
   log indicates the cap was hit. The watermark is advanced to the newest message
   fetched, so the next wake fetches the remainder.

3. **No gateway-resume:** Discord's gateway supports a Resume opcode to replay events
   from a brief disconnection, but the replay window is only ~60 seconds server-side.
   For multi-hour sleeps, the connection drops and reconnects fresh (Identify), losing
   all in-flight events. REST catchup covers this gap losslessly.

4. **Discord-native 25 MB attachment cap:** Larger attachments are rejected with a
   chat-safe error message. This matches Discord's built-in limit (unlike Telegram's
   20 MB or Slack's 20 MB, which are bot-enforced). Verify max file size in your
   Discord client settings.
