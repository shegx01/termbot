mod app;
mod buffer;
mod chat_adapters;
mod command;
mod config;
mod delivery;
mod harness;
mod power;
mod session;
mod state_store;
mod tmux;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use app::App;
use chat_adapters::slack::SlackPlatform;
use chat_adapters::telegram::TelegramAdapter;
use chat_adapters::{ChatPlatform, DiscordAdapter, IncomingMessage};
use config::Config;
use power::types::PowerSignal;
use state_store::{StateStore, StateUpdate};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting termbot...");

    let config_path =
        std::env::var("TERMBOT_CONFIG").unwrap_or_else(|_| "termbot.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded successfully");

    // ── Channels ──────────────────────────────────────────────────────────────
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<IncomingMessage>(100);
    let (state_tx, mut state_rx) = mpsc::channel::<StateUpdate>(64);
    let (power_tx, mut power_rx) = mpsc::channel::<PowerSignal>(32);

    // ── State store ───────────────────────────────────────────────────────────
    let config_path_buf = std::path::PathBuf::from(&config_path);
    let state_path = config
        .power
        .state_file
        .clone()
        .unwrap_or_else(|| StateStore::resolve_default_path(&config_path_buf));
    let store = StateStore::load(&state_path)?;
    tracing::info!("State loaded from {}", state_path.display());

    // ── App ───────────────────────────────────────────────────────────────────
    let mut app = App::new(&config, store, state_tx.clone())?;

    // Startup reconciliation — reconnect surviving sessions
    app.reconcile_startup().await;

    // Emit startup gap banners if last shutdown was unclean with a large gap.
    app.emit_startup_gap_banners();

    // ── Platform adapters ─────────────────────────────────────────────────────
    let telegram: Option<Arc<dyn ChatPlatform>> = if config.telegram_enabled() {
        let tg_config = config.telegram.as_ref().unwrap();
        let tg_user_id = config.auth.telegram_user_id.unwrap();
        let initial_offset = app.initial_telegram_offset();
        let adapter = Arc::new(
            TelegramAdapter::new(
                tg_config.bot_token.clone(),
                tg_user_id,
                config.streaming.edit_throttle_ms,
            )
            .with_initial_offset(initial_offset),
        );
        // Use start_with_state to persist offset updates.
        adapter
            .start_with_state(cmd_tx.clone(), state_tx.clone())
            .await?;
        delivery::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
            app.pending_banner_acks_handle(),
            app.gap_prefix_handle(),
        );
        tracing::info!(
            "Telegram adapter started (user_id={}, initial_offset={})",
            tg_user_id,
            initial_offset
        );
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
        delivery::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
            app.pending_banner_acks_handle(),
            app.gap_prefix_handle(),
        );
        tracing::info!("Slack adapter started (user_id={})", sl_user_id);
        Some(adapter)
    } else {
        tracing::info!("Slack not configured, skipping");
        None
    };

    let discord: Option<Arc<dyn ChatPlatform>> = if config.discord_enabled() {
        let dc_config = config.discord.clone().unwrap();
        let dc_user_id = serenity::all::UserId::new(config.auth.discord_user_id.unwrap());
        let throttle = config.streaming.edit_throttle_ms;
        let adapter = Arc::new(DiscordAdapter::new(dc_config, dc_user_id, throttle)?);
        let adapter_clone = Arc::clone(&adapter);
        let cmd_tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = adapter_clone.start(cmd_tx_clone).await {
                tracing::error!("Discord adapter error: {}", e);
            }
        });
        delivery::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
            app.pending_banner_acks_handle(),
            app.gap_prefix_handle(),
        );
        tracing::info!("Discord adapter enabled");
        Some(adapter)
    } else {
        tracing::info!("Discord not configured, skipping");
        None
    };

    // Log active platforms before moving them into App
    let platforms_desc: Vec<&str> = [
        telegram.as_ref().map(|_| "Telegram"),
        slack.as_ref().map(|_| "Slack"),
        discord.as_ref().map(|_| "Discord"),
    ]
    .into_iter()
    .flatten()
    .collect();

    app.set_platforms(telegram, slack, discord);
    drop(cmd_tx); // Drop our copy so channel closes when adapters stop

    // ── Power subsystem ───────────────────────────────────────────────────────
    if config.power.enabled {
        #[cfg(target_os = "macos")]
        let pm: Arc<dyn power::PowerManager> = Arc::new(power::macos::MacOsPowerManager::new());
        #[cfg(target_os = "linux")]
        let pm: Arc<dyn power::PowerManager> = Arc::new(power::linux::LinuxPowerManager::new());
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let pm: Arc<dyn power::PowerManager> = Arc::new(power::fake::FakePowerManager::new(
            power::types::LidState::Open,
            power::types::PowerSource::Ac,
        ));

        let _supervisor =
            power::supervisor::spawn_power_supervisor(pm, config.power.stayawake_on_battery, None);
        let _gap_detector = power::gap_detector::spawn_gap_detector(power_tx.clone());
        tracing::info!("Power subsystem started");
    } else {
        tracing::info!("Power subsystem disabled (power.enabled=false)");
    }

    // ── Timers ────────────────────────────────────────────────────────────────
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

            // Power signals are highest priority — handle gap before any
            // queued platform message so we don't process stale messages.
            Some(signal) = power_rx.recv() => {
                app.handle_gap(signal).await;
            }

            // State updates — persist before processing commands so chat
            // bindings are durable if we crash immediately after.  Delivery
            // tasks resolve banner-ack oneshots directly via the shared
            // `pending_banner_acks` map — no ack mpsc needed.
            Some(update) = state_rx.recv() => {
                app.apply_state_update(update).await;
            }

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
                app.mark_clean_shutdown().await;
                app.cleanup().await;
                break;
            }
        }
    }

    tracing::info!("termbot stopped");
    Ok(())
}
