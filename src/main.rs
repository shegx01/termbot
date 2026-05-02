mod app;
mod buffer;
mod chat_adapters;
mod command;
mod config;
mod delivery;
mod harness;
mod power;
mod session;
mod socket;
mod state_store;
mod structured_output;
mod tmux;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use app::App;
#[cfg(feature = "slack")]
use chat_adapters::slack::SlackPlatform;
#[cfg(feature = "telegram")]
use chat_adapters::telegram::TelegramAdapter;
#[cfg(feature = "discord")]
use chat_adapters::DiscordAdapter;
use chat_adapters::{ChatPlatform, IncomingMessage};
use config::Config;
use power::types::PowerSignal;
use state_store::{StateStore, StateUpdate};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting terminus...");

    let config_path =
        std::env::var("TERMINUS_CONFIG").unwrap_or_else(|_| "terminus.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded successfully");

    // ── Channels ──────────────────────────────────────────────────────────────
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<IncomingMessage>(100);
    let (state_tx, mut state_rx) = mpsc::channel::<StateUpdate>(256);
    let (power_tx, mut power_rx) = mpsc::channel::<PowerSignal>(32);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<String>(32);
    // `cancel_tx` is cloned only when the socket feature is compiled in.
    // This reference keeps the binding live so non-socket builds don't trip
    // `unused_variables`. Same idiom as `App::handle_gap`'s `cmd_tx`.
    let _ = &cancel_tx;

    // ── State store ───────────────────────────────────────────────────────────
    // Canonicalize config path to absolute so resolve_default_path produces an
    // absolute state file path (avoids "creating tempfile in ." when the binary
    // is launched with a relative TERMINUS_CONFIG).
    let config_path_buf = std::fs::canonicalize(&config_path).unwrap_or_else(|_| {
        // canonicalize failed (e.g. path doesn't exist — shouldn't happen since
        // Config::load already succeeded) — fall back to CWD join.
        let p = std::path::PathBuf::from(&config_path);
        if p.is_relative() {
            std::env::current_dir().map(|cwd| cwd.join(&p)).unwrap_or(p)
        } else {
            p
        }
    });
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

    // ── Platform adapters ─────────────────────────────────────────────────────
    #[cfg(feature = "telegram")]
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
    #[cfg(not(feature = "telegram"))]
    let telegram: Option<Arc<dyn ChatPlatform>> = {
        tracing::info!("Telegram feature disabled at compile time, skipping");
        None
    };

    // Typed Arc for direct catchup calls from App::handle_gap.
    #[cfg(feature = "slack")]
    let slack_platform_typed: Option<Arc<SlackPlatform>>;
    #[cfg(feature = "slack")]
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
        // Seed per-channel watermarks from persisted state before starting.
        adapter
            .seed_watermarks(app.initial_slack_watermarks())
            .await;
        // Use start_with_state so the reconnect loop can persist watermarks.
        adapter
            .start_with_state(cmd_tx.clone(), state_tx.clone())
            .await?;
        delivery::spawn_delivery_task(
            Arc::clone(&adapter) as Arc<dyn ChatPlatform>,
            app.subscribe_stream(),
            app.pending_banner_acks_handle(),
            app.gap_prefix_handle(),
        );
        tracing::info!("Slack adapter started (user_id={})", sl_user_id);
        slack_platform_typed = Some(Arc::clone(&adapter));
        Some(adapter)
    } else {
        tracing::info!("Slack not configured, skipping");
        slack_platform_typed = None;
        None
    };
    #[cfg(not(feature = "slack"))]
    let slack: Option<Arc<dyn ChatPlatform>> = {
        tracing::info!("Slack feature disabled at compile time, skipping");
        None
    };

    // Typed Arc for direct catchup calls from App::handle_gap.
    #[cfg(feature = "discord")]
    let discord_platform_typed: Option<Arc<DiscordAdapter>>;
    #[cfg(feature = "discord")]
    let discord: Option<Arc<dyn ChatPlatform>> = if config.discord_enabled() {
        let dc_config = config.discord.clone().unwrap();
        let dc_user_id = serenity::all::UserId::new(config.auth.discord_user_id.unwrap());
        let throttle = config.streaming.edit_throttle_ms;
        let initial_watermarks = app.initial_discord_watermarks();
        let adapter = Arc::new(DiscordAdapter::new(
            dc_config,
            dc_user_id,
            throttle,
            state_tx.clone(),
            initial_watermarks,
        )?);
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
        discord_platform_typed = Some(Arc::clone(&adapter));
        Some(adapter)
    } else {
        tracing::info!("Discord not configured, skipping");
        discord_platform_typed = None;
        None
    };
    #[cfg(not(feature = "discord"))]
    let discord: Option<Arc<dyn ChatPlatform>> = {
        tracing::info!("Discord feature disabled at compile time, skipping");
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

    app.set_platforms(
        telegram,
        slack,
        #[cfg(feature = "slack")]
        slack_platform_typed,
        discord,
        #[cfg(feature = "discord")]
        discord_platform_typed,
    );

    // Emit startup gap banners AFTER all delivery tasks are spawned and
    // subscribed to the stream channel — otherwise banners are lost.
    app.emit_startup_gap_banners();

    // ── Socket server (optional) ─────────────────────────────────────────────
    let socket_cancel = tokio_util::sync::CancellationToken::new();
    #[cfg(feature = "socket")]
    let socket_drain_secs = config.socket.shutdown_drain_secs;

    #[cfg(feature = "socket")]
    let config_watcher_handle: Option<tokio::task::JoinHandle<()>>;
    #[cfg(feature = "socket")]
    let mut socket_handle: Option<tokio::task::JoinHandle<()>>;

    #[cfg(feature = "socket")]
    {
        // Shared, hot-reloadable client list (atomically swapped on config change)
        let shared_clients: socket::config_watcher::SharedClientList = Arc::new(
            arc_swap::ArcSwap::from_pointee(config.socket.clients.clone()),
        );

        // Shared, in-memory LRU subscription store for restoring subscriptions on reconnect.
        // Cap is read from config (default 100).
        let sub_store_cap = config.socket.max_subscription_store_entries;
        let shared_subs: socket::SharedSubscriptionStore = Arc::new(std::sync::Mutex::new(
            socket::SubscriptionStoreInner::new(sub_store_cap),
        ));

        // Spawn config watcher if socket is enabled.
        // Pass the canonicalized path so the kqueue watcher opens the file
        // directly rather than resolving a relative path to CWD.
        // Pass socket_cancel.clone() so the watcher exits cooperatively on shutdown.
        config_watcher_handle = if config.socket_enabled() {
            socket::config_watcher::spawn_config_watcher(
                config_path_buf.clone(),
                Arc::clone(&shared_clients),
                Arc::clone(&shared_subs),
                socket_cancel.clone(),
            )
        } else {
            None
        };

        socket_handle = if config.socket_enabled() {
            let server = socket::SocketServer::new(
                config.socket.clone(),
                cmd_tx.clone(),
                app.stream_tx_clone(),
                app.ambient_tx_clone(),
                socket_cancel.clone(),
                cancel_tx.clone(),
                Arc::clone(&shared_clients),
                Arc::clone(&shared_subs),
            );
            let handle = tokio::spawn(async move {
                if let Err(e) = server.run().await {
                    tracing::error!("Socket server error: {}", e);
                }
            });
            tracing::info!(
                "Socket server enabled (ws://{}:{})",
                config.socket.bind,
                config.socket.port
            );
            Some(handle)
        } else {
            tracing::info!("Socket server not configured, skipping");
            None
        };
    }
    #[cfg(not(feature = "socket"))]
    let socket_drain_secs: u64 = 0;
    #[cfg(not(feature = "socket"))]
    let config_watcher_handle: Option<tokio::task::JoinHandle<()>> = None;
    #[cfg(not(feature = "socket"))]
    let mut socket_handle: Option<tokio::task::JoinHandle<()>> = None;

    // cmd_tx is kept alive through the loop so handle_gap can pass a clone
    // into each spawned Slack catchup task (tokio::spawn requires ownership).
    // App no longer stores cmd_tx (MAJOR fix removed the field), so the only
    // permanent senders are: this local and the platform-adapter clones.
    // The primary shutdown path is ctrl_c; cmd_tx drops when main() returns.

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
        "terminus ready — listening on {}",
        platforms_desc.join(" and ")
    );

    loop {
        tokio::select! {
            biased;

            // Power signals are highest priority — handle gap before any
            // queued platform message so we don't process stale messages.
            Some(signal) = power_rx.recv() => {
                app.handle_gap(signal, cmd_tx.clone()).await;
            }

            // State updates — persist before processing commands so chat
            // bindings are durable if we crash immediately after.  Delivery
            // tasks resolve banner-ack oneshots directly via the shared
            // `pending_banner_acks` map — no ack mpsc needed.
            Some(update) = state_rx.recv() => {
                app.apply_state_update(update).await;
            }

            // Cancel preempts queued commands — a user interrupt should not
            // wait behind a burst of pending messages (e.g., post-wake drain).
            Some(request_id) = cancel_rx.recv() => {
                if let Err(e) = app.interrupt_foreground().await {
                    tracing::warn!("interrupt_foreground (request_id={}) failed: {}", request_id, e);
                }
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
                // Step 1: Cancel socket server so it can drain connections and
                //         the config watcher can exit cooperatively.
                socket_cancel.cancel();
                // Step 2: Await socket server drain (connection JoinSet drain
                //         happens inside SocketServer::run before it returns).
                // A second Ctrl-C during the drain window force-quits immediately
                // rather than waiting for the full drain timeout.
                if let Some(handle) = socket_handle.take() {
                    let drain_timeout = Duration::from_secs(socket_drain_secs);
                    tokio::select! {
                        biased;
                        _ = tokio::signal::ctrl_c() => {
                            tracing::warn!("Second SIGINT — forcing shutdown without waiting for socket drain");
                        }
                        result = tokio::time::timeout(drain_timeout, handle) => {
                            if result.is_err() {
                                tracing::warn!("Socket drain timed out after {}s", socket_drain_secs);
                            }
                        }
                    }
                }
                // Step 3: Await the config watcher task (it exits on cancel).
                if let Some(handle) = config_watcher_handle {
                    let drain_timeout = Duration::from_secs(socket_drain_secs);
                    if tokio::time::timeout(drain_timeout, handle).await.is_err() {
                        tracing::warn!(
                            "Config watcher did not exit within {}s after cancel",
                            socket_drain_secs
                        );
                    }
                }
                // Step 4: Mark clean shutdown and clean up.
                app.mark_clean_shutdown().await;
                app.cleanup().await;
                break;
            }
        }
    }

    tracing::info!("terminus stopped");
    Ok(())
}
