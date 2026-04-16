//! Subscription registry and filter matcher for ambient events.
//!
//! Each connection has its own `SubscriptionRegistry` that tracks up to
//! `max` named subscriptions. Filter matching uses OR within facets
//! and AND across facets. Empty facets match everything.

use std::collections::HashMap;

use super::envelope::Filter;
use super::events::{AmbientEvent, AmbientEventType};
use crate::buffer::StreamEvent;

/// Per-connection subscription registry.
#[derive(Debug)]
pub struct SubscriptionRegistry {
    subs: HashMap<String, Filter>,
    max: usize,
}

impl SubscriptionRegistry {
    pub fn new(max: usize) -> Self {
        Self {
            subs: HashMap::new(),
            max,
        }
    }

    /// Add a subscription. Returns `Err(())` if the limit is exceeded.
    pub fn add(&mut self, id: String, filter: Filter) -> Result<(), ()> {
        if self.subs.len() >= self.max && !self.subs.contains_key(&id) {
            return Err(());
        }
        self.subs.insert(id, filter);
        Ok(())
    }

    /// Remove a subscription. Returns `Err(())` if not found.
    pub fn remove(&mut self, id: &str) -> Result<(), ()> {
        self.subs.remove(id).map(|_| ()).ok_or(())
    }

    /// Returns all subscription IDs whose filter matches the given ambient event.
    pub fn matches_ambient(&self, event: &AmbientEvent) -> Vec<String> {
        let event_type = event.event_type();
        let event_type_str = ambient_event_type_to_str(&event_type);
        let session = event.session_name();
        let schema = event.schema_name();

        self.subs
            .iter()
            .filter(|(_, filter)| filter_matches(filter, event_type_str, schema, session))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Returns all subscription IDs whose filter matches a StreamEvent.
    /// Used for translating StreamEvent variants (structured_output, webhook_status,
    /// queue_drained) into ambient subscription events at the socket layer.
    pub fn matches_stream(&self, event: &StreamEvent) -> Vec<String> {
        let (event_type_str, schema, session): (&str, Option<&str>, Option<&str>) = match event {
            StreamEvent::StructuredOutputRendered { payload, .. } => {
                ("structured_output", Some(payload.schema.as_str()), None)
            }
            StreamEvent::WebhookStatus { schema, .. } => {
                ("webhook_status", Some(schema.as_str()), None)
            }
            StreamEvent::QueueDrained { .. } => ("queue_drained", None, None),
            StreamEvent::NewMessage { session, .. } => {
                ("session_output", None, Some(session.as_str()))
            }
            StreamEvent::SessionExited { session, .. } => {
                ("session_exited", None, Some(session.as_str()))
            }
            StreamEvent::SessionStarted { session, .. } => {
                ("session_started", None, Some(session.as_str()))
            }
            StreamEvent::GapBanner { .. } => ("gap_banner", None, None),
        };

        self.subs
            .iter()
            .filter(|(_, filter)| filter_matches(filter, event_type_str, schema, session))
            .map(|(id, _)| id.clone())
            .collect()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.subs.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }
}

/// Filter match: OR within each facet, AND across facets.
/// Empty facet = matches everything for that dimension.
fn filter_matches(
    filter: &Filter,
    event_type: &str,
    schema: Option<&str>,
    session: Option<&str>,
) -> bool {
    // Event type facet
    if !filter.event_types.is_empty() && !filter.event_types.iter().any(|t| t == event_type) {
        return false;
    }

    // Schema facet
    if !filter.schemas.is_empty() {
        match schema {
            Some(s) => {
                if !filter.schemas.iter().any(|fs| fs == s) {
                    return false;
                }
            }
            None => return false, // Filter requires schema but event has none
        }
    }

    // Session facet
    if !filter.sessions.is_empty() {
        match session {
            Some(s) => {
                if !filter.sessions.iter().any(|fs| fs == s) {
                    return false;
                }
            }
            None => return false, // Filter requires session but event has none
        }
    }

    true
}

fn ambient_event_type_to_str(t: &AmbientEventType) -> &'static str {
    match t {
        AmbientEventType::SessionOutput => "session_output",
        AmbientEventType::SessionCreated => "session_created",
        AmbientEventType::SessionKilled => "session_killed",
        AmbientEventType::SessionLimitReached => "session_limit_reached",
        AmbientEventType::ChatForward => "chat_forward",
        AmbientEventType::HarnessStarted => "harness_started",
        AmbientEventType::HarnessFinished => "harness_finished",
        AmbientEventType::StructuredOutput => "structured_output",
        AmbientEventType::WebhookStatus => "webhook_status",
        AmbientEventType::QueueDrained => "queue_drained",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_adapters::ChatBinding;

    fn make_session_created(session: &str) -> AmbientEvent {
        AmbientEvent::SessionCreated {
            session: session.to_string(),
            origin_chat: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn make_session_output(session: &str) -> AmbientEvent {
        AmbientEvent::SessionOutput {
            session: session.to_string(),
            chunk: "output".to_string(),
        }
    }

    fn make_chat_forward() -> AmbientEvent {
        AmbientEvent::ChatForward {
            platform: "telegram".to_string(),
            user_id: "123".to_string(),
            text: "hello".to_string(),
        }
    }

    #[test]
    fn empty_filter_matches_all() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add("sub-1".into(), Filter::default()).unwrap();
        let event = make_session_created("build");
        let matches = reg.matches_ambient(&event);
        assert_eq!(matches, vec!["sub-1"]);
    }

    #[test]
    fn event_type_filter() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add(
            "sub-1".into(),
            Filter {
                event_types: vec!["session_created".into()],
                ..Default::default()
            },
        )
        .unwrap();

        // Should match
        let matches = reg.matches_ambient(&make_session_created("build"));
        assert_eq!(matches, vec!["sub-1"]);

        // Should NOT match (different event type)
        let matches = reg.matches_ambient(&make_session_output("build"));
        assert!(matches.is_empty());
    }

    #[test]
    fn session_filter() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add(
            "sub-1".into(),
            Filter {
                sessions: vec!["build".into()],
                ..Default::default()
            },
        )
        .unwrap();

        // Should match
        let matches = reg.matches_ambient(&make_session_output("build"));
        assert_eq!(matches, vec!["sub-1"]);

        // Should NOT match (different session)
        let matches = reg.matches_ambient(&make_session_output("deploy"));
        assert!(matches.is_empty());

        // Should NOT match (no session on event)
        let matches = reg.matches_ambient(&make_chat_forward());
        assert!(matches.is_empty());
    }

    #[test]
    fn facet_and_semantics() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add(
            "sub-1".into(),
            Filter {
                event_types: vec!["session_output".into()],
                sessions: vec!["build".into()],
                ..Default::default()
            },
        )
        .unwrap();

        // Both facets match
        let matches = reg.matches_ambient(&make_session_output("build"));
        assert_eq!(matches, vec!["sub-1"]);

        // Session matches but type doesn't
        let matches = reg.matches_ambient(&make_session_created("build"));
        assert!(matches.is_empty());

        // Type matches but session doesn't
        let matches = reg.matches_ambient(&make_session_output("deploy"));
        assert!(matches.is_empty());
    }

    #[test]
    fn within_facet_or_semantics() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add(
            "sub-1".into(),
            Filter {
                event_types: vec!["session_output".into(), "session_created".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let matches = reg.matches_ambient(&make_session_output("build"));
        assert_eq!(matches, vec!["sub-1"]);

        let matches = reg.matches_ambient(&make_session_created("build"));
        assert_eq!(matches, vec!["sub-1"]);

        // chat_forward doesn't match either event type
        let matches = reg.matches_ambient(&make_chat_forward());
        assert!(matches.is_empty());
    }

    #[test]
    fn overlapping_subscriptions() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add("sub-1".into(), Filter::default()).unwrap(); // match-all
        reg.add(
            "sub-2".into(),
            Filter {
                event_types: vec!["session_created".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let event = make_session_created("build");
        let mut matches = reg.matches_ambient(&event);
        matches.sort();
        assert_eq!(matches, vec!["sub-1", "sub-2"]);
    }

    #[test]
    fn subscription_limit_enforced() {
        let mut reg = SubscriptionRegistry::new(2);
        reg.add("sub-1".into(), Filter::default()).unwrap();
        reg.add("sub-2".into(), Filter::default()).unwrap();
        assert!(reg.add("sub-3".into(), Filter::default()).is_err());
    }

    #[test]
    fn remove_and_readd() {
        let mut reg = SubscriptionRegistry::new(1);
        reg.add("sub-1".into(), Filter::default()).unwrap();
        assert!(reg.add("sub-2".into(), Filter::default()).is_err());
        reg.remove("sub-1").unwrap();
        assert!(reg.add("sub-2".into(), Filter::default()).is_ok());
    }

    #[test]
    fn remove_unknown_fails() {
        let mut reg = SubscriptionRegistry::new(8);
        assert!(reg.remove("nonexistent").is_err());
    }

    #[test]
    fn stream_event_matching() {
        let mut reg = SubscriptionRegistry::new(8);
        reg.add(
            "sub-1".into(),
            Filter {
                event_types: vec!["structured_output".into()],
                schemas: vec!["todos".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let event = StreamEvent::StructuredOutputRendered {
            payload: crate::buffer::StructuredOutputPayload {
                schema: "todos".to_string(),
                value: serde_json::json!({}),
                run_id: "run-1".to_string(),
            },
            chat: ChatBinding {
                platform: crate::chat_adapters::PlatformType::Telegram,
                chat_id: "123".to_string(),
                thread_ts: None,
            },
        };

        let matches = reg.matches_stream(&event);
        assert_eq!(matches, vec!["sub-1"]);

        // Different schema shouldn't match
        let event2 = StreamEvent::StructuredOutputRendered {
            payload: crate::buffer::StructuredOutputPayload {
                schema: "tasks".to_string(),
                value: serde_json::json!({}),
                run_id: "run-2".to_string(),
            },
            chat: ChatBinding {
                platform: crate::chat_adapters::PlatformType::Telegram,
                chat_id: "123".to_string(),
                thread_ts: None,
            },
        };
        let matches2 = reg.matches_stream(&event2);
        assert!(matches2.is_empty());
    }
}
