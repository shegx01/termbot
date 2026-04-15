use std::collections::HashMap;
use std::fmt;

use anyhow::{Context, Result};
use serde_json::Value;

use super::secret::Secret;
use crate::config::SchemaEntry;

/// Resolved per-schema information available at runtime.
///
/// The `SchemaRegistry` is constructed once at startup from `Config::schemas`.
/// It resolves env-var secrets and parses JSON schema strings, failing loudly
/// if any are missing or malformed.
///
/// Custom `Debug` lists schema names but never prints secret bytes.
pub struct SchemaInfo {
    /// Parsed `serde_json::Value` of the JSON Schema.
    pub schema_value: Value,
    /// Webhook URL, if configured for this schema.
    pub webhook_url: Option<String>,
    /// HMAC secret bytes, present iff `webhook_url` is `Some`.
    pub hmac_secret: Option<Secret<Vec<u8>>>,
}

/// Resolved webhook information needed for a single delivery.
///
/// Note: the JSON schema itself is NOT carried here — the wire contract uses
/// raw body + headers, so consumers derive the schema from their own config.
/// The schema is passed to the Claude SDK separately via
/// `SchemaRegistry::schema_value(name)`.
#[derive(Clone)]
pub struct WebhookInfo {
    pub webhook_url: String,
    pub hmac_secret: Secret<Vec<u8>>,
}

/// Registry of all schemas configured in `terminus.toml`.
///
/// Constructed once at startup; the inner map is immutable at runtime
/// (config changes require a restart).
#[derive(Default)]
pub struct SchemaRegistry {
    schemas: HashMap<String, SchemaInfo>,
}

impl fmt::Debug for SchemaRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.schemas.keys().map(|s| s.as_str()).collect();
        let secrets_resolved = self
            .schemas
            .values()
            .filter(|s| s.hmac_secret.is_some())
            .count();
        f.debug_struct("SchemaRegistry")
            .field("schemas", &names)
            .field("secrets_resolved", &secrets_resolved)
            .finish()
    }
}

impl SchemaRegistry {
    /// Build a `SchemaRegistry` from config entries.
    ///
    /// Fails with an actionable error if:
    /// - A schema string is not valid JSON.
    /// - A schema has `webhook` set but `webhook_secret_env` is missing.
    /// - A `webhook_secret_env` names an env var that is not set.
    pub fn from_config(entries: &HashMap<String, SchemaEntry>) -> Result<Self> {
        let mut schemas = HashMap::new();

        for (name, entry) in entries {
            // Parse the JSON Schema string.
            let schema_value: Value = serde_json::from_str(&entry.schema).with_context(|| {
                format!(
                    "[schemas.{}] schema field is not valid JSON: check the TOML multi-line string",
                    name
                )
            })?;

            // Validate webhook / secret consistency.
            let (webhook_url, hmac_secret) = match (&entry.webhook, &entry.webhook_secret_env) {
                (Some(url), Some(env_var)) => {
                    // Resolve the env var at startup.
                    let raw = std::env::var(env_var).with_context(|| {
                        format!(
                            "[schemas.{}] webhook_secret_env refers to env var '{}' which is not set",
                            name, env_var
                        )
                    })?;
                    (Some(url.clone()), Some(Secret::new(raw.into_bytes())))
                }
                (Some(_), None) => {
                    anyhow::bail!(
                        "[schemas.{}] webhook is set but webhook_secret_env is missing; \
                         set webhook_secret_env to the name of an env var containing the HMAC secret",
                        name
                    );
                }
                (None, _) => {
                    // No webhook — render to chat only.
                    (None, None)
                }
            };

            schemas.insert(
                name.clone(),
                SchemaInfo {
                    schema_value,
                    webhook_url,
                    hmac_secret,
                },
            );
        }

        Ok(Self { schemas })
    }

    /// Return a sorted list of schema names (for error messages).
    pub fn schema_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.schemas.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Look up a schema by name, returning the parsed `Value` if found.
    pub fn schema_value(&self, name: &str) -> Option<&Value> {
        self.schemas.get(name).map(|s| &s.schema_value)
    }

    /// Return resolved webhook info for a schema, if it has a webhook configured.
    pub fn webhook_for(&self, name: &str) -> Option<WebhookInfo> {
        let info = self.schemas.get(name)?;
        let url = info.webhook_url.as_ref()?.clone();
        let secret = info.hmac_secret.as_ref()?.clone();
        Some(WebhookInfo {
            webhook_url: url,
            hmac_secret: secret,
        })
    }

    /// Return true if the schema exists in the registry.
    pub fn contains(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn make_entry(schema: &str, webhook: Option<&str>, env_var: Option<&str>) -> SchemaEntry {
        crate::config::SchemaEntry {
            schema: schema.to_string(),
            webhook: webhook.map(|s| s.to_string()),
            webhook_secret_env: env_var.map(|s| s.to_string()),
        }
    }

    #[test]
    fn registry_from_empty_config_is_empty() {
        let reg = SchemaRegistry::from_config(&HashMap::new()).unwrap();
        assert!(reg.schema_names().is_empty());
    }

    #[test]
    fn registry_with_schema_no_webhook() {
        let mut entries = HashMap::new();
        entries.insert(
            "todos".to_string(),
            make_entry(r#"{"type":"object"}"#, None, None),
        );
        let reg = SchemaRegistry::from_config(&entries).unwrap();
        assert!(reg.contains("todos"));
        assert!(reg.webhook_for("todos").is_none());
    }

    #[test]
    fn registry_invalid_json_schema_fails() {
        let mut entries = HashMap::new();
        entries.insert(
            "bad".to_string(),
            make_entry("{not valid json}", None, None),
        );
        let err = SchemaRegistry::from_config(&entries).unwrap_err();
        assert!(err.to_string().contains("[schemas.bad]"));
    }

    #[test]
    fn registry_webhook_without_secret_env_fails() {
        let mut entries = HashMap::new();
        entries.insert(
            "todos".to_string(),
            make_entry(r#"{"type":"object"}"#, Some("https://example.com"), None),
        );
        let err = SchemaRegistry::from_config(&entries).unwrap_err();
        assert!(err.to_string().contains("webhook_secret_env is missing"));
    }

    #[test]
    #[serial(env_mutation)]
    fn registry_webhook_with_missing_env_var_fails() {
        let mut entries = HashMap::new();
        entries.insert(
            "todos".to_string(),
            make_entry(
                r#"{"type":"object"}"#,
                Some("https://example.com"),
                Some("TERMINUS_NONEXISTENT_SECRET_12345"),
            ),
        );
        let err = SchemaRegistry::from_config(&entries).unwrap_err();
        assert!(err
            .to_string()
            .contains("TERMINUS_NONEXISTENT_SECRET_12345"));
    }

    #[test]
    #[serial(env_mutation)]
    fn registry_webhook_with_set_env_var_succeeds() {
        std::env::set_var("TERMINUS_TEST_REGISTRY_SECRET", "mysecret");
        let mut entries = HashMap::new();
        entries.insert(
            "todos".to_string(),
            make_entry(
                r#"{"type":"object"}"#,
                Some("https://example.com"),
                Some("TERMINUS_TEST_REGISTRY_SECRET"),
            ),
        );
        let reg = SchemaRegistry::from_config(&entries).unwrap();
        let wh = reg.webhook_for("todos").unwrap();
        assert_eq!(wh.webhook_url, "https://example.com");
        // Secret bytes should be the env var value
        assert_eq!(wh.hmac_secret.expose(), b"mysecret");
        std::env::remove_var("TERMINUS_TEST_REGISTRY_SECRET");
    }

    #[test]
    #[serial(env_mutation)]
    fn registry_debug_does_not_print_secret_bytes() {
        std::env::set_var("TERMINUS_TEST_REGISTRY_DEBUG_SECRET", "supersecret");
        let mut entries = HashMap::new();
        entries.insert(
            "todos".to_string(),
            make_entry(
                r#"{"type":"object"}"#,
                Some("https://example.com"),
                Some("TERMINUS_TEST_REGISTRY_DEBUG_SECRET"),
            ),
        );
        let reg = SchemaRegistry::from_config(&entries).unwrap();
        let debug_str = format!("{:?}", reg);
        assert!(
            !debug_str.contains("supersecret"),
            "Secret bytes should not appear in Debug output"
        );
        // The debug output should list the schema name but not expose secret bytes
        assert!(
            debug_str.contains("todos"),
            "Schema names should appear in Debug output"
        );
        std::env::remove_var("TERMINUS_TEST_REGISTRY_DEBUG_SECRET");
    }
}
