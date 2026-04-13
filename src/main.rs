mod app;
mod buffer;
mod command;
mod config;
mod harness;
mod platform;
mod power;
mod session;
mod state_store;
mod tmux;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use app::App;
use config::Config;
use platform::slack::SlackPlatform;
use platform::telegram::TelegramAdapter;
use platform::{ChatPlatform, IncomingMessage};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting termbot...");

    let config_path =
        std::env::var("TERMBOT_CONFIG").unwrap_or_else(|_| "termbot.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded successfully");

    let mut app = App::new(&config)?;

    // Startup reconciliation — reconnect surviving sessions
    app.reconcile_startup().await;

    // Channels
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<IncomingMessage>(100);

    // Conditionally create and start platform adapters
    let telegram: Option<Arc<dyn ChatPlatform>> = if config.telegram_enabled() {
        let tg_config = config.telegram.as_ref().unwrap();
        let tg_user_id = config.auth.telegram_user_id.unwrap();
        let adapter = Arc::new(TelegramAdapter::new(
            tg_config.bot_token.clone(),
            tg_user_id,
            config.streaming.edit_throttle_ms,
        ));
        adapter.start(cmd_tx.clone()).await?;
        app::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
        );
        tracing::info!("Telegram adapter started (user_id={})", tg_user_id);
        Some(adapter)
    } else {
        tracing::info!("Telegram not configured, skipping");
        None
    };

    let slack: Option<Arc<dyn ChatPlatform>> = if config.slack_enabled() {
        let sl_config = config.slack.as_ref().unwrap();
        let sl_user_id = config.auth.slack_user_id.as_ref().unwrap().clone();
        let adapter = Arc::new(SlackPlatform::new(
            sl_config.bot_token.clone(),
            sl_config.app_token.clone(),
            sl_config.channel_id.clone(),
            sl_user_id.clone(),
            config.streaming.edit_throttle_ms,
        ));
        let adapter_clone = Arc::clone(&adapter);
        let cmd_tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = adapter_clone.start(cmd_tx_clone).await {
                tracing::error!("Slack adapter error: {}", e);
            }
        });
        app::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
        );
        tracing::info!("Slack adapter started (user_id={})", sl_user_id);
        Some(adapter)
    } else {
        tracing::info!("Slack not configured, skipping");
        None
    };

    // Log active platforms before moving them into App
    let platforms_desc: Vec<&str> = [
        telegram.as_ref().map(|_| "Telegram"),
        slack.as_ref().map(|_| "Slack"),
    ]
    .into_iter()
    .flatten()
    .collect();

    app.set_platforms(telegram, slack);
    drop(cmd_tx); // Drop our copy so channel closes when adapters stop

    // Timers
    let mut health_interval = tokio::time::interval(Duration::from_secs(5));
    let mut poll_interval =
        tokio::time::interval(Duration::from_millis(config.streaming.poll_interval_ms));

    tracing::info!(
        "termbot ready — listening on {}",
        platforms_desc.join(" and ")
    );

    loop {
        tokio::select! {
            biased;

            msg = cmd_rx.recv() => {
                match msg {
                    Some(msg) => app.handle_command(&msg).await,
                    None => {
                        tracing::info!("All platform adapters disconnected, shutting down");
                        break;
                    }
                }
            }

            _ = health_interval.tick() => app.health_check().await,

            _ = poll_interval.tick() => app.poll_output().await,

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutting down...");
                app.cleanup().await;
                break;
            }
        }
    }

    tracing::info!("termbot stopped");
    Ok(())
}
