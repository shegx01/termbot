//! `BannerCoordinator` — owns the per-chat pending-ack and inline-fallback
//! maps, and runs the install/await/fallback dance that `handle_gap` drives.
//!
//! Extracted from `App` because the oneshot/timeout/fallback ordering is
//! the trickiest concurrency invariant in the codebase: the delivery task
//! resolves pending oneshots directly via the shared map (NOT via an mpsc
//! the main loop drains) to avoid deadlocking `handle_gap` inside the main
//! `tokio::select!` branch. Isolating the dance here makes the invariant
//! testable without standing up a full `App`.
//!
//! ## Ownership
//!
//! - `pending_acks: PendingBannerAcks` — shared `Arc<AsyncMutex<HashMap<...>>>`
//!   keyed by chat_id. `install_pending` inserts oneshot senders;
//!   `spawn_delivery_task` (in `src/delivery.rs`) holds a clone of the same
//!   `Arc` and removes+fires the sender on successful banner delivery.
//! - `prefixes: GapPrefixes` — shared `Arc<AsyncMutex<HashMap<...>>>` of
//!   `GapInfo` entries. `await_acks_with_fallback` inserts on timeout;
//!   the delivery task and tests consume entries via `consume_prefix`
//!   (or the shared handle).
//!
//! Both maps are reachable from delivery tasks via the `*_handle` accessors
//! on the App pass-through layer.
//!
//! ## Two-phase contract
//!
//! Callers MUST install pending-acks BEFORE broadcasting `StreamEvent::GapBanner`,
//! and await AFTER. If the broadcast happens first, a fast delivery task
//! could race past the install and the ack would never resolve:
//!
//! ```text
//!   1. coord.install_pending(chat_ids).await       ← lock briefly, insert sender
//!   2. for chat in chats { stream_tx.send(banner) } ← delivery resolves senders
//!   3. coord.await_acks_with_fallback(...).await   ← join_all with timeout
//! ```
//!
//! The two-phase contract relies on a strict locking discipline: every
//! `lock().await` on either map is released before the next `.await`
//! anywhere in the module. No single lock is ever held across the
//! `join_all` await or across the timeout future — the fallback path
//! re-acquires `prefixes` and `pending_acks` in two separate brief critical
//! sections (sequential, never nested) so concurrent delivery tasks racing
//! to resolve a different chat's oneshot are never blocked.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::delivery::{GapInfo, GapPrefixes, PendingBannerAcks};

/// Per-chat banner-ack timeout. After this elapses without the delivery
/// task resolving the oneshot, the inline-prefix fallback engages.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Coordinates banner-ack oneshots and inline-prefix fallbacks across the
/// gap-recovery flow.  All state lives behind `Arc<AsyncMutex<...>>` so
/// delivery tasks (running in their own spawned tasks) can resolve pending
/// oneshots directly without routing through the main loop.
pub struct BannerCoordinator {
    pending_acks: PendingBannerAcks,
    prefixes: GapPrefixes,
}

impl Default for BannerCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl BannerCoordinator {
    pub fn new() -> Self {
        Self {
            pending_acks: Arc::new(AsyncMutex::new(HashMap::new())),
            prefixes: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Shared handle to the pending-ack map.  Passed to `spawn_delivery_task`
    /// so the delivery task can resolve oneshots directly.
    pub fn pending_acks_handle(&self) -> PendingBannerAcks {
        Arc::clone(&self.pending_acks)
    }

    /// Shared handle to the inline-prefix map.  Passed to `spawn_delivery_task`
    /// so the delivery task can consume `[gap: …]` markers set on timeout.
    pub fn prefixes_handle(&self) -> GapPrefixes {
        Arc::clone(&self.prefixes)
    }

    /// Install fresh oneshot pending-acks for `chat_ids`.  Returns receivers
    /// paired with their chat IDs for the caller to pass into
    /// [`Self::await_acks_with_fallback`] after broadcasting banners.
    ///
    /// If a pending entry already exists for a chat (e.g. a second
    /// `handle_gap` arrived while the first was still awaiting), the
    /// previous oneshot sender is dropped — the prior `rx.await` will see
    /// `Err(RecvError)` and engage the inline-prefix fallback for that chat.
    /// Acceptable single-user semantics: the user still sees a banner for
    /// the second gap.
    pub async fn install_pending(
        &self,
        chat_ids: &[String],
    ) -> Vec<(String, oneshot::Receiver<()>)> {
        let mut pending = self.pending_acks.lock().await;
        let mut out = Vec::with_capacity(chat_ids.len());
        for chat_id in chat_ids {
            let (tx, rx) = oneshot::channel::<()>();
            pending.insert(chat_id.clone(), tx);
            out.push((chat_id.clone(), rx));
        }
        out
    }

    /// Wait for each pending-ack to resolve within [`ACK_TIMEOUT`].  Awaits
    /// all chats concurrently so a slow delivery task on one platform does
    /// not delay others.
    ///
    /// On timeout or dropped sender, registers a `GapInfo` in the prefix
    /// map (so the next outbound message for that chat is rendered with
    /// `[gap: Xm Ys] `) and clears the stale pending-ack entry.
    pub async fn await_acks_with_fallback(
        &self,
        futures: Vec<(String, oneshot::Receiver<()>)>,
        gap: Duration,
        paused_at: DateTime<Utc>,
        resumed_at: DateTime<Utc>,
    ) {
        let results = join_all(futures.into_iter().map(|(chat_id, rx)| async move {
            let result = tokio::time::timeout(ACK_TIMEOUT, rx).await;
            (chat_id, result)
        }))
        .await;

        for (chat_id, result) in results {
            match result {
                Ok(Ok(())) => {
                    tracing::info!(
                        "GapBanner delivered for chat_id={} (gap={:.0}s)",
                        chat_id,
                        gap.as_secs_f64()
                    );
                }
                Ok(Err(_)) | Err(_) => {
                    // `Ok(Err(_))` = sender dropped (delivery task died);
                    // `Err(_)` = ACK_TIMEOUT elapsed without an ack.
                    tracing::warn!(
                        "GapBanner delivery timed out for chat_id={} — \
                         using inline fallback prefix",
                        chat_id
                    );
                    self.prefixes.lock().await.insert(
                        chat_id.clone(),
                        GapInfo {
                            gap,
                            paused_at,
                            resumed_at,
                        },
                    );
                    self.pending_acks.lock().await.remove(&chat_id);
                }
            }
        }
    }

    /// Consume and return the inline gap prefix for `chat_id` if one is
    /// pending.  The delivery task consumes entries directly via
    /// [`Self::prefixes_handle`]; this method is also reached through
    /// `App::consume_gap_prefix` for testing.
    pub async fn consume_prefix(&self, chat_id: &str) -> Option<GapInfo> {
        self.prefixes.lock().await.remove(chat_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn install_pending_returns_one_receiver_per_chat() {
        let coord = BannerCoordinator::new();
        let chats = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let futures = coord.install_pending(&chats).await;

        assert_eq!(futures.len(), 3);
        let chat_ids: Vec<&str> = futures.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(chat_ids, vec!["a", "b", "c"]);

        // Map state mirrors the install.
        let acks = coord.pending_acks_handle();
        let pending = acks.lock().await;
        assert_eq!(pending.len(), 3);
        assert!(pending.contains_key("a"));
        assert!(pending.contains_key("b"));
        assert!(pending.contains_key("c"));
    }

    #[tokio::test]
    async fn install_pending_replaces_existing_sender() {
        // Second install for the same chat must replace the first sender.
        // The first `rx.await` then resolves with `Err(RecvError)` — that
        // path engages the inline-prefix fallback in `await_acks_*`.
        let coord = BannerCoordinator::new();
        let chats = vec!["chat1".to_string()];

        let first = coord.install_pending(&chats).await;
        let (_, first_rx) = first.into_iter().next().unwrap();

        let _second = coord.install_pending(&chats).await;

        // First sender was dropped on the replace; first rx errors out.
        let result = tokio::time::timeout(Duration::from_millis(50), first_rx).await;
        match result {
            Ok(Err(_)) => { /* expected: sender dropped */ }
            other => panic!("expected Ok(Err(_)) from dropped sender, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn await_resolves_when_delivery_fires_oneshot() {
        let coord = BannerCoordinator::new();
        let chats = vec!["chat1".to_string()];
        let futures = coord.install_pending(&chats).await;

        // Simulate the delivery task: lock the shared map, remove the
        // sender, and fire it.
        let acks = coord.pending_acks_handle();
        tokio::spawn(async move {
            if let Some(tx) = acks.lock().await.remove("chat1") {
                let _ = tx.send(());
            }
        });

        let now = Utc::now();
        coord
            .await_acks_with_fallback(futures, Duration::from_secs(60), now, now)
            .await;

        // No fallback should have been registered.
        assert!(coord.consume_prefix("chat1").await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn await_registers_inline_fallback_on_timeout() {
        // Mock-time test: install, do not resolve, advance past 5s.  The
        // coordinator must register a GapInfo and remove the pending entry.
        let coord = BannerCoordinator::new();
        let chats = vec!["chat1".to_string()];
        let futures = coord.install_pending(&chats).await;

        let paused = Utc::now() - chrono::Duration::seconds(120);
        let resumed = Utc::now();
        let gap = Duration::from_secs(120);

        let coord_arc = Arc::new(coord);
        let coord_clone = Arc::clone(&coord_arc);
        let handle = tokio::spawn(async move {
            coord_clone
                .await_acks_with_fallback(futures, gap, paused, resumed)
                .await;
        });

        // Advance past the 5s ack timeout.
        tokio::time::advance(Duration::from_secs(6)).await;
        handle.await.expect("await task should complete");

        // Inline fallback must be set with the gap info.
        let info = coord_arc
            .consume_prefix("chat1")
            .await
            .expect("inline fallback should be registered after timeout");
        assert_eq!(info.gap, gap);
        // Pending entry must be cleaned up.
        assert!(coord_arc.pending_acks_handle().lock().await.is_empty());
    }

    #[tokio::test]
    async fn await_handles_dropped_sender_via_fallback() {
        // If the delivery task panics or is dropped, the oneshot sender
        // is dropped without being fired.  `rx.await` then yields
        // `Err(RecvError)` immediately — the fallback path must engage.
        let coord = BannerCoordinator::new();
        let chats = vec!["chat1".to_string()];
        let futures = coord.install_pending(&chats).await;

        // Drop the sender to simulate a dead delivery task.
        coord.pending_acks_handle().lock().await.remove("chat1");

        let now = Utc::now();
        let gap = Duration::from_secs(90);
        coord.await_acks_with_fallback(futures, gap, now, now).await;

        let info = coord
            .consume_prefix("chat1")
            .await
            .expect("inline fallback should be registered when sender is dropped");
        assert_eq!(info.gap, gap);
    }

    #[tokio::test(start_paused = true)]
    async fn await_handles_mixed_outcomes_per_chat() {
        // Two chats: one's oneshot fires before the timeout, the other
        // never fires. The resolved chat must NOT have a fallback entry;
        // the timed-out chat must have one. Pre-extraction this case was
        // untested at any layer — `join_all` is what makes per-chat
        // outcomes independent, so a regression that conflated chats
        // would pass the existing all-success / all-fail tests.
        let coord = Arc::new(BannerCoordinator::new());
        let chats = vec!["fast".to_string(), "slow".to_string()];
        let futures = coord.install_pending(&chats).await;

        // Resolve only the "fast" chat's oneshot synchronously before
        // entering the await; the "slow" chat's sender stays installed.
        let acks = coord.pending_acks_handle();
        if let Some(tx) = acks.lock().await.remove("fast") {
            let _ = tx.send(());
        }

        let paused = Utc::now() - chrono::Duration::seconds(120);
        let resumed = Utc::now();
        let gap = Duration::from_secs(120);

        let coord_clone = Arc::clone(&coord);
        let handle = tokio::spawn(async move {
            coord_clone
                .await_acks_with_fallback(futures, gap, paused, resumed)
                .await;
        });

        tokio::time::advance(Duration::from_secs(6)).await;
        handle.await.expect("await task should complete");

        // Resolved chat: no fallback.
        assert!(
            coord.consume_prefix("fast").await.is_none(),
            "resolved chat must not have a fallback entry"
        );
        // Timed-out chat: fallback registered.
        let info = coord
            .consume_prefix("slow")
            .await
            .expect("timed-out chat must have a fallback entry");
        assert_eq!(info.gap, gap);
    }

    #[tokio::test]
    async fn install_pending_releases_lock_before_returning() {
        // Regression guard for the lock-release-before-broadcast invariant
        // (see module-level "Two-phase contract"). If a future refactor
        // accidentally held the lock across the return — by, say, returning
        // a `MutexGuard` instead of a `Vec` — `try_lock` would block here.
        let coord = BannerCoordinator::new();
        let chats = vec!["chat1".to_string(), "chat2".to_string()];
        let _futures = coord.install_pending(&chats).await;

        // Non-blocking poll on the same shared map: must succeed.
        let acks = coord.pending_acks_handle();
        let guard = acks
            .try_lock()
            .expect("pending_acks lock must be released after install_pending returns");
        assert_eq!(guard.len(), 2);
    }

    #[tokio::test]
    async fn install_pending_with_empty_slice_is_a_no_op() {
        // `handle_gap` reaches this path on wake when no platform has any
        // active chats yet. The coordinator must not panic, must not
        // deadlock on the lock, and must return an empty Vec.
        let coord = BannerCoordinator::new();
        let futures = coord.install_pending(&[]).await;
        assert!(futures.is_empty());
        assert!(coord.pending_acks_handle().lock().await.is_empty());
    }

    #[tokio::test]
    async fn consume_prefix_returns_and_clears() {
        let coord = BannerCoordinator::new();
        let now = Utc::now();
        coord.prefixes_handle().lock().await.insert(
            "chat42".to_string(),
            GapInfo {
                gap: Duration::from_secs(90),
                paused_at: now - chrono::Duration::seconds(90),
                resumed_at: now,
            },
        );

        let prefix = coord.consume_prefix("chat42").await;
        assert!(prefix.is_some());
        assert_eq!(prefix.unwrap().gap, Duration::from_secs(90));

        // Second call returns None: the entry was cleared.
        assert!(coord.consume_prefix("chat42").await.is_none());
    }
}
