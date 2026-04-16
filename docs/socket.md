# Socket API Reference (terminus/v1)

Terminus exposes an authenticated WebSocket endpoint for local programs and remote
agents to drive it programmatically. This is an opt-in feature; enable it with
`[socket] enabled = true` in `terminus.toml`.

## Quick Start

```toml
# terminus.toml
[socket]
enabled = true

[[socket.client]]
name = "my-agent"
token = "tk_live_your-secret-token-here"
```

```bash
# Connect with websocat
websocat ws://127.0.0.1:7645 -H "Authorization: Bearer tk_live_your-secret-token-here"

# Send a command
{"type":"request","request_id":"req-1","command":": list"}
# Response: {"type":"ack","request_id":"req-1",...}
# Response: {"type":"result","request_id":"req-1","value":{"text":"No active sessions"},...}
# Response: {"type":"end","request_id":"req-1"}
```

## Transport

- **Protocol:** Plain `ws://` (WebSocket over TCP)
- **TLS:** Terminated externally by a reverse proxy (nginx, caddy, Traefik, K8s
  ingress). Terminus does not handle TLS itself.
- **Default bind:** `127.0.0.1:7645`
- **Framing:** One JSON envelope per WebSocket text message

## Authentication

Bearer token in the `Authorization` header at the HTTP upgrade:

```
GET /socket HTTP/1.1
Upgrade: websocket
Authorization: Bearer <client-token>
```

Tokens are configured per-client in `terminus.toml`:

```toml
[[socket.client]]
name = "agent-a"
token = "tk_live_..."

[[socket.client]]
name = "dashboard"
token = "tk_live_..."
```

- **Missing/invalid token:** HTTP 401 at upgrade; no WebSocket established.
- **Client identity:** The `name` field appears in tracing logs and the `hello_ack`
  envelope. Each client has a distinct token for revocation.

## Wire Protocol

### Client -> Server Envelopes

#### `hello` (optional)
```json
{"type": "hello", "protocol": "terminus/v1"}
```
Server validates protocol version. If unsupported, sends `error` + closes.

#### `request`
```json
{
  "type": "request",
  "request_id": "client-supplied-ulid",
  "command": ": claude --schema=todos list tasks",
  "options": {"stream_tokens": false}
}
```
- `request_id`: Client-supplied correlation ID (echoed on all response frames)
- `command`: Any valid terminus command (`: list`, `: new foo`, `: claude ...`, etc.)
- `options.stream_tokens`: If `true`, emit `text_chunk` frames during Claude streaming

#### `cancel`
```json
{"type": "cancel", "request_id": "client-supplied-ulid"}
```
Best-effort cancellation. Claude turns cancel at the next event boundary; shell
commands already dispatched to tmux cannot be cancelled (result carries `cancelled: true`).

#### `subscribe`
```json
{
  "type": "subscribe",
  "subscription_id": "sub-1",
  "filter": {
    "event_types": ["structured_output", "session_output"],
    "schemas": ["todos"],
    "sessions": ["build"]
  }
}
```
- **Filter facets:** `event_types`, `schemas`, `sessions` (all optional)
- **Semantics:** OR within a facet, AND across facets. Empty facet = match all.
- **Multiple subscriptions:** Up to `max_subscriptions_per_connection` (default 8)
  per connection, each with its own `subscription_id` and filter.

#### `unsubscribe`
```json
{"type": "unsubscribe", "subscription_id": "sub-1"}
```

#### `ping`
```json
{"type": "ping"}
```
Server responds with `pong`. (Server also sends WebSocket-level Ping frames.)

### Server -> Client Envelopes

#### `hello_ack`
```json
{
  "type": "hello_ack",
  "session_id": "server-ulid",
  "client_name": "agent-a",
  "protocol": "terminus/v1",
  "capabilities": ["pipelining", "subscriptions", "cancel"]
}
```
Sent immediately on successful connection.

#### Per-Request Lifecycle

For each `request`, the server emits frames in this order:

```
ack -> [text_chunk | tool_call | tool_result | partial_result]* -> (result | error) -> end
```

**`ack`** — Request accepted:
```json
{"type": "ack", "request_id": "...", "accepted_at": "2026-04-15T12:00:01Z"}
```

**`text_chunk`** — Claude streaming text (only if `stream_tokens: true`):
```json
{"type": "text_chunk", "request_id": "...", "stream": "response", "chunk": "..."}
```

**`tool_call`** — Claude invoked a tool:
```json
{"type": "tool_call", "request_id": "...", "tool": "bash", "params": {...}}
```

**`tool_result`** — Tool returned:
```json
{"type": "tool_result", "request_id": "...", "tool": "bash", "ok": true, "value": {...}}
```
On failure: `"ok": false, "error": "description"` (value may be absent).

**`partial_result`** — Intermediate structured output draft (before final validation):
```json
{"type": "partial_result", "request_id": "...", "schema": "todos", "value": {"partial": true}}
```

**`result`** — Terminal success:
```json
{
  "type": "result",
  "request_id": "...",
  "schema": "todos",
  "value": {"todos": [...]},
  "run_id": "01J...",
  "cancelled": false
}
```

**`error`** — Terminal error:
```json
{"type": "error", "request_id": "...", "code": "unknown_command", "message": "..."}
```

For `rate_limited` errors, the response includes a machine-readable `retry_after_ms`:
```json
{"type": "error", "code": "rate_limited", "message": "retry after 100ms", "retry_after_ms": 100}
```

**`end`** — End-of-response sentinel:
```json
{"type": "end", "request_id": "..."}
```

#### Subscription Events

**`subscribed`** / **`unsubscribed`** — Subscription lifecycle:
```json
{"type": "subscribed", "subscription_id": "sub-1"}
```

**`event`** — Ambient event matching a subscription filter:
```json
{
  "type": "event",
  "subscription_id": "sub-1",
  "event": {
    "type": "session_created",
    "session": "build",
    "created_at": "2026-04-15T12:00:01Z"
  }
}
```

#### Ambient Event Types

| Event Type | Tags | Source |
|---|---|---|
| `structured_output` | `schema`, `run_id`, `chat`, `value` | Claude `--schema` result |
| `webhook_status` | `schema`, `run_id`, `status`, `chat` | Webhook delivery attempt |
| `queue_drained` | `delivered_count`, `chat` | Webhook queue drain cycle |
| `session_output` | `session`, `chunk` | tmux capture-pane output |
| `session_created` | `session`, `origin_chat`, `created_at` | `: new <name>` |
| `session_killed` | `session`, `reason`, `killed_at` | `: kill <name>` |
| `session_limit_reached` | `attempted`, `current`, `max` | Session cap hit |
| `chat_forward` | `platform`, `user_id`, `text` | Chat message received |
| `harness_started` | `harness`, `run_id` | Claude turn started |
| `harness_finished` | `harness`, `run_id`, `status` | Claude turn completed |

#### Warnings & Lifecycle

**`warning`** — Non-fatal (e.g., broadcast lag):
```json
{"type": "warning", "code": "lagged", "missed_count": 42}
```

**`shutting_down`** — Server shutting down:
```json
{"type": "shutting_down", "drain_deadline_ms": 30000}
```

**`pong`** — Response to client `ping`:
```json
{"type": "pong"}
```

### Error Codes

| Code | Meaning |
|---|---|
| `unsupported_protocol` | Client requested an unknown protocol version |
| `rate_limited` | Token bucket exhausted; retry after indicated time |
| `queue_full` | Per-connection pending request limit reached |
| `unknown_command` | Command not recognized |
| `parse_error` | Malformed JSON envelope |
| `schema_validation_failed` | Claude output didn't match schema |
| `subscription_limit` | Max subscriptions per connection exceeded |
| `unknown_subscription` | Unsubscribe for non-existent subscription_id |
| `message_too_large` | Inbound message exceeds `max_message_bytes` |
| `internal_error` | Unexpected server error |

## Pipelining

Clients can send multiple `request` envelopes without waiting for results. The
server queues them per-connection (FIFO, up to `max_pending_requests`). Each
response set is tagged with its own `request_id`.

```
-> {"type":"request","request_id":"a","command":": list"}
-> {"type":"request","request_id":"b","command":": new build"}
<- {"type":"ack","request_id":"a",...}
<- {"type":"result","request_id":"a",...}
<- {"type":"end","request_id":"a"}
<- {"type":"ack","request_id":"b",...}
<- {"type":"result","request_id":"b",...}
<- {"type":"end","request_id":"b"}
```

## Configuration Reference

All fields in `[socket]` with defaults:

```toml
[socket]
enabled = false                       # Must be true to start the listener
bind = "127.0.0.1"                    # Bind address ("0.0.0.0" for containers)
port = 7645                           # Listener port
max_connections = 16                  # Concurrent WebSocket connections
max_subscriptions_per_connection = 8  # Named subscriptions per connection
max_pending_requests = 32             # Pipelined request queue depth
rate_limit_per_second = 20.0          # Token refill rate (requests/sec)
rate_limit_burst = 60.0               # Token bucket capacity (burst)
max_message_bytes = 1048576           # 1 MiB inbound message limit
ping_interval_secs = 30               # Server-sent WebSocket Ping interval
pong_timeout_secs = 10                # Close if Pong not received in time
idle_timeout_secs = 300               # Close if no inbound activity
send_buffer_size = 1024               # Per-connection broadcast buffer
shutdown_drain_secs = 30              # Grace period for in-flight requests
```

## Reverse Proxy Examples

### nginx

```nginx
upstream terminus_ws {
    server 127.0.0.1:7645;
}

server {
    listen 443 ssl;
    server_name terminus.example.com;

    ssl_certificate     /etc/letsencrypt/live/terminus.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/terminus.example.com/privkey.pem;

    location / {
        proxy_pass http://terminus_ws;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
```

### Caddy

```caddyfile
terminus.example.com {
    reverse_proxy 127.0.0.1:7645
}
```

Caddy handles WebSocket upgrade + TLS automatically.

## Troubleshooting

**Connection refused:**
- Check `[socket] enabled = true` in `terminus.toml`
- Verify at least one `[[socket.client]]` is configured
- Check bind address and port (`ss -tlnp | grep 7645`)

**401 Unauthorized:**
- Check `Authorization: Bearer <token>` header is present and correct
- Token must match a `[[socket.client]]` entry exactly

**Rate limited:**
- Increase `rate_limit_burst` and `rate_limit_per_second`
- Or slow down request rate

**Lagged warnings:**
- The broadcast buffer overflowed (slow consumer). Events were dropped.
- Increase `send_buffer_size` or process events faster
- This is non-fatal; the connection continues

**Idle timeout:**
- Send periodic `ping` envelopes or real commands to prevent timeout
- Increase `idle_timeout_secs` for long-idle monitoring sessions

## v1 Limitations

- **No config hot-reload.** Token changes require terminus restart.
- **No persistent subscriptions.** Reconnect re-subscribes from scratch.
- **No binary frames.** Inbound attachments (images/files) not supported over socket.
- **Shell command cancellation is best-effort.** Once dispatched to tmux, the command
  runs to completion; only the result delivery is suppressed.
