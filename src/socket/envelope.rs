//! Wire protocol envelope types for the terminus WebSocket API.
//!
//! One JSON envelope per WebSocket text message. Every envelope has a `type`
//! discriminator. Per-request frames echo the client-supplied `request_id`.
//! Ambient events carry `subscription_id`.

use serde::{Deserialize, Serialize};

// ─── Inbound (Client → Server) ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundEnvelope {
    /// Optional handshake (server validates, echoes capabilities).
    Hello {
        #[serde(default)]
        protocol: Option<String>,
    },
    /// Command request with client-supplied correlation ID.
    Request {
        request_id: String,
        command: String,
        #[allow(dead_code)] // parsed from wire, used when stream_tokens is implemented
        #[serde(default)]
        options: Option<RequestOptions>,
    },
    /// Cancel an in-flight request.
    Cancel { request_id: String },
    /// Subscribe to ambient events with a filter.
    Subscribe {
        subscription_id: String,
        #[serde(default)]
        filter: Filter,
    },
    /// Unsubscribe from ambient events.
    Unsubscribe { subscription_id: String },
    /// Liveness ping (optional — server also sends WS Ping frames).
    Ping,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // parsed from wire, used when Claude streaming is wired
pub struct RequestOptions {
    /// If true, emit `text_chunk` frames during Claude streaming.
    #[serde(default)]
    pub stream_tokens: bool,
}

// ─── Outbound (Server → Client) ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundEnvelope {
    /// Handshake response.
    HelloAck {
        session_id: String,
        client_name: String,
        protocol: String,
        capabilities: Vec<String>,
    },

    // ── Per-request frames (ordered: ack → intermediates → terminal → end) ──
    /// Server accepted the request.
    Ack {
        request_id: String,
        accepted_at: String,
    },
    /// Streaming text chunk from Claude (only if `stream_tokens: true`).
    #[allow(dead_code)] // constructed when Claude streaming is wired to socket
    TextChunk {
        request_id: String,
        stream: String,
        chunk: String,
    },
    /// Claude invoked a tool.
    #[allow(dead_code)] // constructed when Claude tool events are wired to socket
    ToolCall {
        request_id: String,
        tool: String,
        params: serde_json::Value,
    },
    /// Claude received a tool result.
    #[allow(dead_code)] // constructed when Claude tool events are wired to socket
    ToolResult {
        request_id: String,
        tool: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Intermediate structured output draft.
    #[allow(dead_code)] // constructed when structured output streaming is wired
    PartialResult {
        request_id: String,
        schema: String,
        value: serde_json::Value,
    },
    /// Terminal success frame.
    Result {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        value: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        cancelled: bool,
    },
    /// Terminal error frame.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        code: ErrorCode,
        message: String,
        /// Machine-readable retry hint (milliseconds). Present on `rate_limited`.
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// End-of-response sentinel.
    End { request_id: String },

    // ── Subscription lifecycle ───────────────────────────────────────────────
    /// Subscription confirmed.
    Subscribed { subscription_id: String },
    /// Subscription removed.
    Unsubscribed { subscription_id: String },
    /// Ambient event delivered to a matching subscription.
    Event {
        subscription_id: String,
        event: serde_json::Value,
    },

    // ── Warnings (non-fatal) ────────────────────────────────────────────────
    /// Broadcast receiver lagged or other non-fatal warning.
    Warning {
        #[serde(skip_serializing_if = "Option::is_none")]
        subscription_id: Option<String>,
        code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        missed_count: Option<u64>,
    },

    // ── Lifecycle ───────────────────────────────────────────────────────────
    /// Server is shutting down; drain in progress.
    ShuttingDown { drain_deadline_ms: u64 },
    /// Response to client Ping.
    Pong,
}

/// Error codes per the wire protocol spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedProtocol,
    RateLimited,
    QueueFull,
    UnknownCommand,
    ParseError,
    SchemaValidationFailed,
    SubscriptionLimit,
    UnknownSubscription,
    MessageTooLarge,
    InternalError,
}

/// Subscription filter: empty facets match everything (OR within facet, AND across facets).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub schemas: Vec<String>,
    #[serde(default)]
    pub sessions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_request_deserializes() {
        let json = r#"{"type":"request","request_id":"abc-123","command":": list"}"#;
        let env: InboundEnvelope = serde_json::from_str(json).unwrap();
        match env {
            InboundEnvelope::Request {
                request_id,
                command,
                ..
            } => {
                assert_eq!(request_id, "abc-123");
                assert_eq!(command, ": list");
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn inbound_subscribe_with_filter_deserializes() {
        let json = r#"{
            "type": "subscribe",
            "subscription_id": "sub-1",
            "filter": {
                "event_types": ["session_output"],
                "sessions": ["build"]
            }
        }"#;
        let env: InboundEnvelope = serde_json::from_str(json).unwrap();
        match env {
            InboundEnvelope::Subscribe {
                subscription_id,
                filter,
            } => {
                assert_eq!(subscription_id, "sub-1");
                assert_eq!(filter.event_types, vec!["session_output"]);
                assert_eq!(filter.sessions, vec!["build"]);
                assert!(filter.schemas.is_empty());
            }
            other => panic!("expected Subscribe, got {:?}", other),
        }
    }

    #[test]
    fn inbound_cancel_deserializes() {
        let json = r#"{"type":"cancel","request_id":"req-42"}"#;
        let env: InboundEnvelope = serde_json::from_str(json).unwrap();
        match env {
            InboundEnvelope::Cancel { request_id } => {
                assert_eq!(request_id, "req-42");
            }
            other => panic!("expected Cancel, got {:?}", other),
        }
    }

    #[test]
    fn inbound_hello_deserializes() {
        let json = r#"{"type":"hello","protocol":"terminus/v1"}"#;
        let env: InboundEnvelope = serde_json::from_str(json).unwrap();
        assert!(matches!(env, InboundEnvelope::Hello { .. }));
    }

    #[test]
    fn inbound_ping_deserializes() {
        let json = r#"{"type":"ping"}"#;
        let env: InboundEnvelope = serde_json::from_str(json).unwrap();
        assert!(matches!(env, InboundEnvelope::Ping));
    }

    #[test]
    fn outbound_hello_ack_serializes() {
        let env = OutboundEnvelope::HelloAck {
            session_id: "sess-1".into(),
            client_name: "agent-a".into(),
            protocol: "terminus/v1".into(),
            capabilities: vec!["pipelining".into(), "subscriptions".into()],
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"hello_ack""#));
        assert!(json.contains(r#""session_id":"sess-1""#));
    }

    #[test]
    fn outbound_ack_serializes() {
        let env = OutboundEnvelope::Ack {
            request_id: "req-1".into(),
            accepted_at: "2026-04-15T12:00:01Z".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"ack""#));
        assert!(json.contains(r#""request_id":"req-1""#));
    }

    #[test]
    fn outbound_result_serializes() {
        let env = OutboundEnvelope::Result {
            request_id: "req-1".into(),
            schema: Some("todos".into()),
            value: serde_json::json!({"items": []}),
            run_id: Some("01J123".into()),
            cancelled: false,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"result""#));
        assert!(json.contains(r#""schema":"todos""#));
    }

    #[test]
    fn outbound_error_serializes() {
        let env = OutboundEnvelope::Error {
            request_id: Some("req-1".into()),
            code: ErrorCode::UnknownCommand,
            message: "Unknown command".into(),
            retry_after_ms: None,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"error""#));
        assert!(json.contains(r#""code":"unknown_command""#));
        // retry_after_ms should be absent (skip_serializing_if)
        assert!(!json.contains("retry_after_ms"));
    }

    #[test]
    fn outbound_error_with_retry_after_serializes() {
        let env = OutboundEnvelope::Error {
            request_id: None,
            code: ErrorCode::RateLimited,
            message: "retry after 100ms".into(),
            retry_after_ms: Some(100),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""retry_after_ms":100"#));
    }

    #[test]
    fn outbound_end_serializes() {
        let env = OutboundEnvelope::End {
            request_id: "req-1".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"end""#));
    }

    #[test]
    fn outbound_event_serializes() {
        let env = OutboundEnvelope::Event {
            subscription_id: "sub-1".into(),
            event: serde_json::json!({"type": "session_created", "session": "build"}),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""subscription_id":"sub-1""#));
    }

    #[test]
    fn outbound_warning_serializes() {
        let env = OutboundEnvelope::Warning {
            subscription_id: Some("sub-1".into()),
            code: "lagged".into(),
            missed_count: Some(42),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"warning""#));
        assert!(json.contains(r#""missed_count":42"#));
    }

    #[test]
    fn outbound_shutting_down_serializes() {
        let env = OutboundEnvelope::ShuttingDown {
            drain_deadline_ms: 30000,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"shutting_down""#));
    }

    #[test]
    fn outbound_pong_serializes() {
        let env = OutboundEnvelope::Pong;
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"pong""#));
    }

    #[test]
    fn filter_defaults_to_empty() {
        let filter = Filter::default();
        assert!(filter.event_types.is_empty());
        assert!(filter.schemas.is_empty());
        assert!(filter.sessions.is_empty());
    }

    #[test]
    fn error_codes_roundtrip() {
        let codes = vec![
            ErrorCode::UnsupportedProtocol,
            ErrorCode::RateLimited,
            ErrorCode::QueueFull,
            ErrorCode::UnknownCommand,
            ErrorCode::ParseError,
            ErrorCode::SchemaValidationFailed,
            ErrorCode::SubscriptionLimit,
            ErrorCode::UnknownSubscription,
            ErrorCode::MessageTooLarge,
            ErrorCode::InternalError,
        ];
        for code in codes {
            let json = serde_json::to_string(&code).unwrap();
            let restored: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&restored).unwrap(),
                json,
                "ErrorCode roundtrip failed for {:?}",
                code
            );
        }
    }
}
