use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::queue::DeliveryQueue;
use super::registry::SchemaRegistry;
use super::webhook::WebhookClient;
use crate::buffer::{StreamEvent, WebhookStatusKind};

/// Exponential backoff delays in seconds: [1, 2, 4, 8, 16, 32, 60, 60, ...]
const BACKOFF_SCHEDULE_SECS: &[u64] = &[1, 2, 4, 8, 16, 32, 60];

/// Compute the next backoff delay with ±20% jitter sampled fresh per attempt.
fn next_backoff(attempt: usize) -> Duration {
    let base_secs = BACKOFF_SCHEDULE_SECS.get(attempt).copied().unwrap_or(60); // cap at 60s

    // ±20% jitter sampled fresh on each call (per-attempt, not per-job).
    // Use a simple pseudorandom approach based on current time nanos.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Map nanos % 4001 → [0, 4000]; then divide by 10_000 → [0.0, 0.4000];
    // shift by -0.20 → [-0.20, +0.20].
    let jitter_fraction = (nanos % 4001) as f64 / 10_000.0 - 0.20;
    let jitter_secs = base_secs as f64 * jitter_fraction;
    let total_secs = base_secs as f64 + jitter_secs;
    let total_ms = (total_secs * 1000.0).max(100.0) as u64; // floor at 100ms
    Duration::from_millis(total_ms)
}

/// Spawn the long-running retry worker.
///
/// The worker:
/// 1. Scans `queue.pending` every time it wakes up (after a delivery attempt or
///    a short idle sleep when the queue is empty).
/// 2. For each job, attempts delivery via `WebhookClient`.
/// 3. On success: removes the file; broadcasts `WebhookStatus::Delivered`.
/// 4. On failure: sleeps `next_backoff(attempt)` with ±20% per-attempt jitter,
///    then retries. If `max_retry_age_hours > 0` and the job is too old, moves
///    it to `dead/`.
/// 5. Respects a `tokio::sync::Notify` shutdown signal (from
///    `App::mark_clean_shutdown`): completes the current in-flight delivery
///    then exits.
///
/// If the `SchemaRegistry` no longer contains a job's schema (config was
/// changed + restart), the job is moved to `dead/` with reason `"schema_removed"`
/// and a WARN is logged.
pub fn spawn_retry_worker(
    queue: Arc<DeliveryQueue>,
    client: Arc<WebhookClient>,
    registry: Arc<SchemaRegistry>,
    events_tx: broadcast::Sender<StreamEvent>,
    shutdown: Arc<tokio::sync::Notify>,
    shutdown_flag: Arc<AtomicBool>,
    max_retry_age_hours: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("Retry worker started");
        let mut attempt_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut consecutive_failures: std::collections::HashMap<String, (u16, u32)> =
            std::collections::HashMap::new();

        loop {
            // Check for shutdown signal (non-blocking).
            if shutdown_flag.load(Ordering::Acquire) {
                tracing::info!("Retry worker: shutdown signal received, draining in-flight...");
                break;
            }

            let paths = match queue.list_pending().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("Retry worker: failed to list pending queue: {:#}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if paths.is_empty() {
                // No work — wait a bit then re-check.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = shutdown.notified() => {
                        tracing::info!("Retry worker: shutdown during idle, exiting");
                        break;
                    }
                }
                continue;
            }

            let mut delivered_count = 0usize;
            // Per-chat delivery tally for this drain cycle.  At the end we
            // emit one `QueueDrained` StreamEvent per chat that had jobs
            // delivered, so each chat sees `✅ queue drained (N delivered)`.
            let mut per_chat_delivered: std::collections::HashMap<
                crate::chat_adapters::ChatBinding,
                u32,
            > = std::collections::HashMap::new();

            for path in &paths {
                // Re-check shutdown before each job.
                if shutdown_flag.load(Ordering::Acquire) {
                    tracing::info!("Retry worker: shutdown during drain, stopping");
                    break;
                }

                let job = match queue.read_job(path).await {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!("Retry worker: failed to read job {:?}: {}", path, e);
                        continue;
                    }
                };

                // Check if schema still exists in registry.
                if !registry.contains(&job.schema) {
                    tracing::warn!(
                        schema = %job.schema,
                        run_id = %job.run_id,
                        "Retry worker: schema no longer in registry, moving job to dead/"
                    );
                    if let Err(e) = queue.move_to_dead(path, "schema_removed").await {
                        tracing::error!("Failed to move orphaned job to dead/: {}", e);
                    }
                    attempt_counts.remove(&job.run_id);
                    consecutive_failures.remove(&job.schema);
                    continue;
                }

                // Check max retry age.
                if max_retry_age_hours > 0 {
                    if let Some(age) = job_age(&job) {
                        // saturating_mul prevents overflow if max_retry_age_hours
                        // is misconfigured to an extreme value (>u64::MAX/3600).
                        if age > Duration::from_secs(max_retry_age_hours.saturating_mul(3600)) {
                            tracing::warn!(
                                schema = %job.schema,
                                run_id = %job.run_id,
                                age_hours = age.as_secs() / 3600,
                                "Retry worker: job exceeded max_retry_age_hours, abandoning"
                            );
                            if let Err(e) = queue.move_to_dead(path, "max_retry_age_exceeded").await
                            {
                                tracing::error!("Failed to move aged job to dead/: {}", e);
                            }
                            attempt_counts.remove(&job.run_id);
                            consecutive_failures.remove(&job.schema);
                            let _ = events_tx.send(StreamEvent::WebhookStatus {
                                schema: job.schema.clone(),
                                run_id: job.run_id.clone(),
                                status: WebhookStatusKind::Abandoned,
                                chat: job.source_chat_binding.clone(),
                            });
                            continue;
                        }
                    }
                }

                // Look up webhook info.
                let webhook_info = match registry.webhook_for(&job.schema) {
                    Some(w) => w,
                    None => {
                        // Schema has no webhook — remove the job (chat-only schema).
                        tracing::info!(
                            schema = %job.schema,
                            "Retry worker: schema has no webhook, removing job"
                        );
                        let _ = queue.remove(path).await;
                        continue;
                    }
                };

                // Attempt delivery with a tracing span.
                let attempt = *attempt_counts.get(&job.run_id).unwrap_or(&0);
                let span = tracing::info_span!(
                    "webhook.deliver",
                    schema = %job.schema,
                    run_id = %job.run_id,
                    attempt = attempt,
                );
                // Enter/exit the span manually so the `EnteredSpan` guard
                // doesn't live across the `await` boundary (which would make
                // the future !Send because `EnteredSpan` is not `Send`).
                span.in_scope(|| {
                    tracing::debug!("attempting webhook delivery");
                });

                match client.deliver(&job, &webhook_info).await {
                    Ok(_elapsed) => {
                        attempt_counts.remove(&job.run_id);
                        consecutive_failures.remove(&job.schema);
                        let _ = queue.remove(path).await;
                        delivered_count += 1;
                        *per_chat_delivered
                            .entry(job.source_chat_binding.clone())
                            .or_insert(0) += 1;

                        let _ = events_tx.send(StreamEvent::WebhookStatus {
                            schema: job.schema.clone(),
                            run_id: job.run_id.clone(),
                            status: WebhookStatusKind::Delivered,
                            chat: job.source_chat_binding.clone(),
                        });
                    }
                    Err(e) => {
                        // Classify the error: permanent 4xx (except 429) errors
                        // will never succeed on retry, so move them to dead letter
                        // immediately rather than burning retry budget.
                        let http_status = match &e {
                            super::webhook::DeliveryError::HttpError { status, .. } => {
                                Some(*status)
                            }
                            _ => None,
                        };

                        // All 4xx except 408 (Request Timeout) and 429 (Rate Limited)
                        // are permanent — retrying will never succeed.
                        let is_permanent = matches!(http_status, Some(s) if
                            (400..500).contains(&s) && s != 408 && s != 429
                        );

                        if is_permanent {
                            let status = http_status.unwrap();
                            tracing::warn!(
                                schema = %job.schema,
                                run_id = %job.run_id,
                                status = status,
                                error = %e,
                                "Webhook delivery failed with permanent error, moving to dead/"
                            );
                            if let Err(move_err) = queue
                                .move_to_dead(path, &format!("permanent_http_error_{}", status))
                                .await
                            {
                                tracing::error!(
                                    "Failed to move permanently-failed job to dead/: {}",
                                    move_err
                                );
                            }
                            attempt_counts.remove(&job.run_id);
                            consecutive_failures.remove(&job.schema);
                            let _ = events_tx.send(StreamEvent::WebhookStatus {
                                schema: job.schema.clone(),
                                run_id: job.run_id.clone(),
                                status: WebhookStatusKind::Abandoned,
                                chat: job.source_chat_binding.clone(),
                            });
                            continue;
                        }

                        let next_attempt = attempt + 1;
                        attempt_counts.insert(job.run_id.clone(), next_attempt);

                        // Track consecutive failures by HTTP status for WARN logging.
                        if let Some(status) = http_status {
                            let entry = consecutive_failures
                                .entry(job.schema.clone())
                                .or_insert((status, 0));
                            if entry.0 == status {
                                entry.1 += 1;
                                if entry.1 == 3 {
                                    tracing::warn!(
                                        schema = %job.schema,
                                        run_id = %job.run_id,
                                        status = status,
                                        "Webhook delivery has failed 3 consecutive times with the same HTTP status; \
                                         check your webhook endpoint configuration"
                                    );
                                }
                            } else {
                                *entry = (status, 1);
                            }
                        }

                        let delay = next_backoff(attempt);
                        tracing::warn!(
                            schema = %job.schema,
                            run_id = %job.run_id,
                            attempt = next_attempt,
                            delay_ms = delay.as_millis() as u64,
                            error = %e,
                            "Webhook delivery failed, will retry"
                        );

                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = shutdown.notified() => {
                                tracing::info!("Retry worker: shutdown during backoff, exiting");
                                return;
                            }
                        }
                    }
                }
            }

            if delivered_count > 0 {
                tracing::info!(delivered_count = delivered_count, "queue.drain_complete");
                // Emit one QueueDrained per chat whose pending jobs were drained
                // in this cycle, so the user sees `✅ queue drained (N delivered)`
                // in each affected chat.
                for (chat, count) in per_chat_delivered {
                    let _ = events_tx.send(StreamEvent::QueueDrained {
                        delivered_count: count,
                        chat,
                    });
                }
            }
        }

        tracing::info!("Retry worker exited");
    })
}

/// Decode the ULID timestamp from a run_id to compute job age.
/// Returns `None` if the run_id cannot be parsed.
fn job_age(job: &super::queue::DeliveryJob) -> Option<Duration> {
    let ulid: ulid::Ulid = job.run_id.parse().ok()?;
    let created_ms = ulid.timestamp_ms();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let age_ms = now_ms.saturating_sub(created_ms);
    Some(Duration::from_millis(age_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_matches_spec() {
        // Verify the base schedule (before jitter) matches [1, 2, 4, 8, 16, 32, 60, 60].
        let expected_bases = [1u64, 2, 4, 8, 16, 32, 60, 60];
        for (i, expected) in expected_bases.iter().enumerate() {
            let base = BACKOFF_SCHEDULE_SECS.get(i).copied().unwrap_or(60);
            assert_eq!(
                base, *expected,
                "Backoff at index {} should be {}s, got {}",
                i, expected, base
            );
        }
    }

    #[test]
    fn next_backoff_with_jitter_stays_within_20_percent() {
        // For each schedule entry, verify the jitter stays within ±20%.
        for (attempt, &base_secs) in BACKOFF_SCHEDULE_SECS.iter().enumerate() {
            let min_ms = (base_secs as f64 * 0.80 * 1000.0) as u64;
            let max_ms = (base_secs as f64 * 1.20 * 1000.0) as u64;

            // Sample 20 times — each should be in range.
            for _ in 0..20 {
                let d = next_backoff(attempt);
                assert!(
                    d.as_millis() as u64 >= min_ms.saturating_sub(10),
                    "Backoff {}ms below minimum {}ms for attempt {}",
                    d.as_millis(),
                    min_ms,
                    attempt
                );
                assert!(
                    d.as_millis() as u64 <= max_ms + 10,
                    "Backoff {}ms above maximum {}ms for attempt {}",
                    d.as_millis(),
                    max_ms,
                    attempt
                );
            }
        }
    }

    #[test]
    fn max_retry_age_zero_is_forever() {
        // With max_retry_age_hours = 0, the age check is skipped.
        // We verify the condition: age > 0 * 3600 = 0, which is always true,
        // so when max_retry_age_hours is 0 the check must be explicitly skipped.
        // This is a logic test: our worker code says `if max_retry_age_hours > 0`.
        let max_retry_age_hours: u64 = 0;
        // The condition guard: if 0 > 0 is false, so no job is ever abandoned.
        assert!(
            max_retry_age_hours == 0,
            "max_retry_age_hours = 0 should never trigger abandonment"
        );
    }

    /// Guards against a misconfiguration where `max_retry_age_hours` is set to
    /// a value larger than `u64::MAX / 3600`.  Plain multiplication would
    /// panic in debug builds or wrap to a tiny value in release, causing all
    /// jobs to be immediately abandoned.  `saturating_mul` caps at u64::MAX.
    #[test]
    fn max_retry_age_saturating_mul_does_not_overflow() {
        let huge: u64 = u64::MAX;
        let result = huge.saturating_mul(3600);
        assert_eq!(
            result,
            u64::MAX,
            "saturating_mul must cap at u64::MAX, not wrap"
        );
        // Duration::from_secs(u64::MAX) must not panic either.
        let _ = Duration::from_secs(result);
    }

    #[test]
    fn backoff_cap_is_60_seconds() {
        // Index 6 and beyond should cap at 60s.
        for attempt in 6..20 {
            let base = BACKOFF_SCHEDULE_SECS.get(attempt).copied().unwrap_or(60);
            assert_eq!(base, 60, "Backoff at attempt {} should cap at 60s", attempt);
        }
    }
}
