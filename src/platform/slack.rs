use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use super::{ChatPlatform, IncomingMessage, PlatformMessageId, PlatformType, ReplyContext};

pub struct SlackPlatform {
    bot_token: String,
    app_token: String,
    channel_id: String,
    authorized_user_id: String,
    connected: Arc<AtomicBool>,
    #[allow(dead_code)]
    rate_limit_ms: u64,
    http_client: reqwest::Client,
    #[allow(dead_code)]
    last_edit: Arc<Mutex<Option<Instant>>>,
}

impl SlackPlatform {
    pub fn new(
        bot_token: String,
        app_token: String,
        channel_id: String,
        authorized_user_id: String,
        rate_limit_ms: u64,
    ) -> Self {
        Self {
            bot_token,
            app_token,
            channel_id,
            authorized_user_id,
            connected: Arc::new(AtomicBool::new(false)),
            rate_limit_ms,
            http_client: reqwest::Client::new(),
            last_edit: Arc::new(Mutex::new(None)),
        }
    }

    async fn get_websocket_url(&self) -> Result<String> {
        let resp = self
            .http_client
            .post("https://slack.com/api/apps.connections.open")
            .header("Authorization", format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await
            .context("Failed to call apps.connections.open")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse apps.connections.open response")?;

        if body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let url = body
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("apps.connections.open response missing 'url' field"))?;
            Ok(url.to_string())
        } else {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("apps.connections.open failed: {}", err))
        }
    }

    async fn run_websocket_loop(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let ws_url = self.get_websocket_url().await?;
        info!("Connecting to Slack Socket Mode (url redacted for security)");

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("Failed to connect to Slack WebSocket")?;

        self.connected.store(true, Ordering::SeqCst);
        info!("Slack WebSocket connected");

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        while let Some(msg_result) = ws_source.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("WebSocket receive error: {}", e);
                    break;
                }
            };

            match msg {
                Message::Text(text) => {
                    debug!("Received Slack WS message ({} bytes)", text.len());
                    let payload: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Failed to parse Slack WS JSON: {}", e);
                            continue;
                        }
                    };

                    // Acknowledge with envelope_id if present
                    if let Some(envelope_id) =
                        payload.get("envelope_id").and_then(|v| v.as_str())
                    {
                        let ack = serde_json::json!({ "envelope_id": envelope_id });
                        if let Err(e) = ws_sink.send(Message::Text(ack.to_string())).await {
                            error!("Failed to send Slack WS ack: {}", e);
                            break;
                        }
                    }

                    // Handle hello (connection confirmation)
                    if payload.get("type").and_then(|v| v.as_str()) == Some("hello") {
                        info!("Slack Socket Mode hello received");
                        continue;
                    }

                    // Handle events_api messages
                    if payload.get("type").and_then(|v| v.as_str()) == Some("events_api") {
                        if let Some(incoming) = self.parse_event(&payload) {
                            if cmd_tx.send(incoming).await.is_err() {
                                warn!("Command channel closed, stopping Slack listener");
                                break;
                            }
                        }
                    }
                }
                Message::Ping(data) => {
                    if let Err(e) = ws_sink.send(Message::Pong(data)).await {
                        error!("Failed to send WebSocket pong: {}", e);
                        break;
                    }
                }
                Message::Close(_) => {
                    info!("Slack WebSocket closed by server");
                    break;
                }
                _ => {}
            }
        }

        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn parse_event(&self, payload: &serde_json::Value) -> Option<IncomingMessage> {
        let event = payload.get("payload")?.get("event")?;

        if event.get("type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }

        // Ignore bot messages and message subtypes (edits, deletes, joins, etc.)
        if event.get("bot_id").is_some() {
            return None;
        }
        if event.get("subtype").is_some() {
            return None;
        }

        let user = event.get("user").and_then(|v| v.as_str())?;
        let text = event.get("text").and_then(|v| v.as_str())?;
        let channel = event
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.channel_id);
        let thread_ts = event
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Auth check — silently drop unauthorized users
        if user != self.authorized_user_id {
            debug!("Ignoring message from unauthorized Slack user: {}", user);
            return None;
        }

        Some(IncomingMessage {
            user_id: user.to_string(),
            text: text.to_string(),
            platform: PlatformType::Slack,
            reply_context: ReplyContext {
                platform: PlatformType::Slack,
                chat_id: channel.to_string(),
                thread_ts,
            },
        })
    }
}

#[async_trait]
impl ChatPlatform for SlackPlatform {
    async fn start(&self, cmd_tx: mpsc::Sender<IncomingMessage>) -> Result<()> {
        const INITIAL_DELAY_SECS: u64 = 5;
        const MAX_DELAY_SECS: u64 = 300;

        let mut delay_secs = INITIAL_DELAY_SECS;

        loop {
            match self.run_websocket_loop(cmd_tx.clone()).await {
                Ok(()) => {
                    // Clean disconnect — reset backoff
                    delay_secs = INITIAL_DELAY_SECS;
                    info!(
                        "Slack WebSocket loop ended, reconnecting in {}s...",
                        delay_secs
                    );
                }
                Err(e) => {
                    error!(
                        "Slack WebSocket loop error: {}. Reconnecting in {}s...",
                        e, delay_secs
                    );
                }
            }

            self.connected.store(false, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;

            // Exponential backoff with cap
            delay_secs = (delay_secs * 2).min(MAX_DELAY_SECS);

            // Stop reconnecting if the receiver has been dropped
            if cmd_tx.is_closed() {
                info!("Command channel closed, stopping Slack reconnect loop");
                break;
            }
        }

        Ok(())
    }

    async fn send_message(
        &self,
        text: &str,
        chat_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<PlatformMessageId> {
        let mut body = serde_json::json!({
            "channel": chat_id,
            "text": text,
        });

        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let resp = self
            .http_client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .context("Failed to call chat.postMessage")?;

        let result: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse chat.postMessage response")?;

        if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let ts = result
                .get("ts")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("chat.postMessage response missing 'ts' field"))?;
            Ok(PlatformMessageId::Slack(ts.to_string()))
        } else {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("chat.postMessage failed: {}", err))
        }
    }

    async fn edit_message(
        &self,
        msg_id: &PlatformMessageId,
        chat_id: &str,
        text: &str,
    ) -> Result<()> {
        let ts = match msg_id {
            PlatformMessageId::Slack(ts) => ts,
            _ => {
                return Err(anyhow!(
                    "edit_message called with non-Slack PlatformMessageId"
                ))
            }
        };

        // Rate limiting: release lock before sleeping to avoid blocking concurrent calls
        let sleep_dur = {
            let mut last = self.last_edit.lock().await;
            let dur = if let Some(t) = *last {
                let elapsed = t.elapsed();
                let min_gap = Duration::from_millis(self.rate_limit_ms);
                if elapsed < min_gap { Some(min_gap - elapsed) } else { None }
            } else {
                None
            };
            *last = Some(Instant::now());
            dur
        };
        if let Some(d) = sleep_dur {
            tokio::time::sleep(d).await;
        }

        let body = serde_json::json!({
            "channel": chat_id,
            "ts": ts,
            "text": text,
        });

        let resp = self
            .http_client
            .post("https://slack.com/api/chat.update")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .json(&body)
            .send()
            .await
            .context("Failed to call chat.update")?;

        let result: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse chat.update response")?;

        if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("chat.update failed: {}", err))
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn platform_type(&self) -> PlatformType {
        PlatformType::Slack
    }
}
