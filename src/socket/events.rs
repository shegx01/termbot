//! Ambient event types for the socket subscription bus.
//!
//! These 7 event types represent genuinely new observability events that
//! don't duplicate existing `StreamEvent` variants. The connection task
//! subscribes to both `broadcast::Receiver<StreamEvent>` (for structured-output,
//! webhook-status, queue-drained) and `broadcast::Receiver<AmbientEvent>` (for these).

use serde::{Deserialize, Serialize};

/// Discriminator enum for subscription filter matching on event types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbientEventType {
    SessionOutput,
    SessionCreated,
    SessionKilled,
    SessionLimitReached,
    ChatForward,
    HarnessStarted,
    HarnessFinished,
    // These 3 are StreamEvent variants translated at the socket layer,
    // but listed here so subscription filters can reference them.
    StructuredOutput,
    WebhookStatus,
    QueueDrained,
}

/// Ambient events emitted on the socket subscription bus.
///
/// Carried on `broadcast::channel<AmbientEvent>` (capacity 512).
/// All variants are emitted from `App` methods, not from `session.rs`
/// or `harness/mod.rs` (preserving zero-change to those modules).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AmbientEvent {
    #[allow(dead_code)] // translated from StreamEvent::NewMessage at socket layer
    SessionOutput { session: String, chunk: String },
    SessionCreated {
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin_chat: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    },
    SessionKilled {
        session: String,
        reason: String,
        killed_at: chrono::DateTime<chrono::Utc>,
    },
    SessionLimitReached {
        attempted: String,
        current: usize,
        max: usize,
    },
    ChatForward {
        platform: String,
        user_id: String,
        text: String,
    },
    HarnessStarted {
        harness: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_hash: Option<String>,
    },
    HarnessFinished {
        harness: String,
        run_id: String,
        status: String,
    },
}

impl AmbientEvent {
    /// Returns the event type discriminator for subscription matching.
    pub fn event_type(&self) -> AmbientEventType {
        match self {
            Self::SessionOutput { .. } => AmbientEventType::SessionOutput,
            Self::SessionCreated { .. } => AmbientEventType::SessionCreated,
            Self::SessionKilled { .. } => AmbientEventType::SessionKilled,
            Self::SessionLimitReached { .. } => AmbientEventType::SessionLimitReached,
            Self::ChatForward { .. } => AmbientEventType::ChatForward,
            Self::HarnessStarted { .. } => AmbientEventType::HarnessStarted,
            Self::HarnessFinished { .. } => AmbientEventType::HarnessFinished,
        }
    }

    /// Returns the session name if this event is session-scoped.
    pub fn session_name(&self) -> Option<&str> {
        match self {
            Self::SessionOutput { session, .. }
            | Self::SessionCreated { session, .. }
            | Self::SessionKilled { session, .. }
            | Self::SessionLimitReached {
                attempted: session, ..
            } => Some(session),
            _ => None,
        }
    }

    /// Returns the schema name if applicable (always None for AmbientEvent;
    /// schema-bearing events come from StreamEvent translation).
    pub fn schema_name(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_dispatch() {
        let cases: Vec<(AmbientEvent, AmbientEventType)> = vec![
            (
                AmbientEvent::SessionOutput {
                    session: "s".into(),
                    chunk: "c".into(),
                },
                AmbientEventType::SessionOutput,
            ),
            (
                AmbientEvent::SessionCreated {
                    session: "s".into(),
                    origin_chat: None,
                    created_at: chrono::Utc::now(),
                },
                AmbientEventType::SessionCreated,
            ),
            (
                AmbientEvent::SessionKilled {
                    session: "s".into(),
                    reason: "r".into(),
                    killed_at: chrono::Utc::now(),
                },
                AmbientEventType::SessionKilled,
            ),
            (
                AmbientEvent::SessionLimitReached {
                    attempted: "a".into(),
                    current: 10,
                    max: 10,
                },
                AmbientEventType::SessionLimitReached,
            ),
            (
                AmbientEvent::ChatForward {
                    platform: "tg".into(),
                    user_id: "u".into(),
                    text: "t".into(),
                },
                AmbientEventType::ChatForward,
            ),
            (
                AmbientEvent::HarnessStarted {
                    harness: "h".into(),
                    run_id: "r".into(),
                    prompt_hash: None,
                },
                AmbientEventType::HarnessStarted,
            ),
            (
                AmbientEvent::HarnessFinished {
                    harness: "h".into(),
                    run_id: "r".into(),
                    status: "ok".into(),
                },
                AmbientEventType::HarnessFinished,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(event.event_type(), expected, "for {:?}", event);
        }
    }

    #[test]
    fn session_name_returns_some_for_session_scoped() {
        let ev = AmbientEvent::SessionOutput {
            session: "build".into(),
            chunk: "x".into(),
        };
        assert_eq!(ev.session_name(), Some("build"));

        let ev = AmbientEvent::SessionCreated {
            session: "deploy".into(),
            origin_chat: None,
            created_at: chrono::Utc::now(),
        };
        assert_eq!(ev.session_name(), Some("deploy"));
    }

    #[test]
    fn session_name_returns_none_for_non_session() {
        let ev = AmbientEvent::ChatForward {
            platform: "tg".into(),
            user_id: "u".into(),
            text: "t".into(),
        };
        assert_eq!(ev.session_name(), None);

        let ev = AmbientEvent::HarnessStarted {
            harness: "h".into(),
            run_id: "r".into(),
            prompt_hash: None,
        };
        assert_eq!(ev.session_name(), None);
    }

    #[test]
    fn schema_name_always_none() {
        let ev = AmbientEvent::SessionOutput {
            session: "s".into(),
            chunk: "c".into(),
        };
        assert_eq!(ev.schema_name(), None);
    }

    #[test]
    fn serde_serialization_roundtrip() {
        let ev = AmbientEvent::SessionCreated {
            session: "build".into(),
            origin_chat: Some("telegram:123".into()),
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"session_created""#));
        assert!(json.contains(r#""session":"build""#));
    }
}
