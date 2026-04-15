use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat_adapters::ChatBinding;

/// A delivery job stored on disk under `<queue_dir>/pending/<run_id>.json`.
///
/// The job is self-describing: it carries the webhook URL, HMAC components,
/// and the originating chat binding so it can be fully retried across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryJob {
    /// Schema name (used for routing and logging).
    pub schema: String,
    /// Validated structured output value (sent as the POST body).
    pub value: Value,
    /// ULID run ID (used as filename and `X-Terminus-Run-Id` header).
    pub run_id: String,
    /// Originating chat binding for retry-worker status messages.
    pub source_chat_binding: ChatBinding,
}

/// Disk-backed delivery queue under `<queue_dir>/`.
///
/// Directory layout:
/// ```
/// <queue_dir>/
///   tmp/       temporary files (write target before atomic rename)
///   pending/   durably enqueued jobs waiting for delivery
///   dead/      jobs that could not be delivered (schema removed / age exceeded)
/// ```
///
/// All `tmp/` → `pending/` renames are atomic (same-device, checked at startup).
pub struct DeliveryQueue {
    tmp_dir: PathBuf,
    pending_dir: PathBuf,
    dead_dir: PathBuf,
}

impl DeliveryQueue {
    /// Create (or open) a queue rooted at `queue_dir`.
    ///
    /// Creates `tmp/`, `pending/`, and `dead/` subdirectories if absent.
    /// On Unix, verifies that `tmp/` and `pending/` are on the same device
    /// (required for atomic rename semantics). Returns `Err` with an
    /// explanatory message if the check fails (e.g. NFS/FUSE mount).
    pub fn new(queue_dir: PathBuf) -> Result<Self> {
        let tmp_dir = queue_dir.join("tmp");
        let pending_dir = queue_dir.join("pending");
        let dead_dir = queue_dir.join("dead");

        // Create directories.
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("Failed to create queue tmp dir: {}", tmp_dir.display()))?;
        std::fs::create_dir_all(&pending_dir).with_context(|| {
            format!(
                "Failed to create queue pending dir: {}",
                pending_dir.display()
            )
        })?;
        std::fs::create_dir_all(&dead_dir)
            .with_context(|| format!("Failed to create queue dead dir: {}", dead_dir.display()))?;

        // Restrict directory permissions to owner-only (0o700) on Unix.
        // Queue job files contain structured-output bodies that may include
        // information derived from the user's code/context; limiting to the
        // owner prevents other users on shared hosts from reading them.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&tmp_dir, &pending_dir, &dead_dir] {
                let perms = std::fs::Permissions::from_mode(0o700);
                if let Err(e) = std::fs::set_permissions(dir, perms) {
                    tracing::warn!(
                        "Failed to set 0o700 perms on queue subdir {}: {} (continuing)",
                        dir.display(),
                        e
                    );
                }
            }
        }

        // Same-device check on Unix (atomic rename requires same filesystem).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let tmp_dev = std::fs::metadata(&tmp_dir)
                .with_context(|| format!("Failed to stat tmp dir: {}", tmp_dir.display()))?
                .dev();
            let pending_dev = std::fs::metadata(&pending_dir)
                .with_context(|| format!("Failed to stat pending dir: {}", pending_dir.display()))?
                .dev();
            if tmp_dev != pending_dev {
                anyhow::bail!(
                    "queue_dir '{}' spans multiple filesystems: tmp/ and pending/ are on different \
                     devices ({} vs {}). queue_dir must be on a single local filesystem for \
                     atomic rename semantics (NFS, CIFS, and FUSE filesystems are not supported).",
                    queue_dir.display(),
                    tmp_dev,
                    pending_dev
                );
            }
        }

        Ok(Self {
            tmp_dir,
            pending_dir,
            dead_dir,
        })
    }

    /// Atomically enqueue a `DeliveryJob`.
    ///
    /// Writes to `tmp/<run_id>.json` first, then renames into `pending/<run_id>.json`.
    /// Because `tmp/` and `pending/` are on the same device (verified in `new`),
    /// the rename is atomic on Linux/macOS. A crash after the write but before
    /// the rename leaves a file in `tmp/` which is cleaned up on next restart.
    ///
    /// Returns the path of the resulting file in `pending/`.
    pub async fn enqueue(&self, job: &DeliveryJob) -> Result<PathBuf> {
        let filename = format!("{}.json", job.run_id);
        let tmp_path = self.tmp_dir.join(&filename);
        let pending_path = self.pending_dir.join(&filename);

        // Serialize the job.
        let json_bytes = serde_json::to_vec_pretty(job)
            .with_context(|| format!("Failed to serialize job {}", job.run_id))?;

        // Write to tmp.
        tokio::fs::write(&tmp_path, &json_bytes)
            .await
            .with_context(|| format!("Failed to write job to tmp: {}", tmp_path.display()))?;

        // Restrict file permissions to owner-only (0o600) on Unix.  Job files
        // contain structured-output bodies (derived from user code/context).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = tokio::fs::set_permissions(&tmp_path, perms).await {
                tracing::warn!(
                    "Failed to set 0o600 perms on {}: {} (continuing)",
                    tmp_path.display(),
                    e
                );
            }
        }

        // fsync the file to ensure the write is durable before rename.
        let file = tokio::fs::File::open(&tmp_path).await.with_context(|| {
            format!("Failed to open tmp file for fsync: {}", tmp_path.display())
        })?;
        file.sync_all()
            .await
            .with_context(|| format!("Failed to fsync tmp file: {}", tmp_path.display()))?;
        drop(file);

        // Atomic rename into pending/.
        tokio::fs::rename(&tmp_path, &pending_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to rename {} → {}",
                    tmp_path.display(),
                    pending_path.display()
                )
            })?;

        // fsync the pending directory so the rename itself is durable.
        // On Linux this is required: without it, a power loss after the rename
        // but before the dirent write-back could leave the file on disk but
        // unreachable via its pending/ path.  macOS handles this implicitly but
        // the extra fsync is safe (at worst a no-op).
        let pending_dir_file = tokio::fs::File::open(&self.pending_dir)
            .await
            .with_context(|| {
                format!(
                    "Failed to open pending dir for fsync: {}",
                    self.pending_dir.display()
                )
            })?;
        pending_dir_file.sync_all().await.with_context(|| {
            format!(
                "Failed to fsync pending dir: {}",
                self.pending_dir.display()
            )
        })?;
        drop(pending_dir_file);

        let count = self.pending_count().await.unwrap_or(0);
        tracing::info!(
            schema = %job.schema,
            run_id = %job.run_id,
            count = count,
            "queue.enqueue"
        );

        Ok(pending_path)
    }

    /// Return all pending job paths sorted by filename (ULID lexicographic order = creation order).
    pub async fn list_pending(&self) -> Result<Vec<PathBuf>> {
        let mut entries = tokio::fs::read_dir(&self.pending_dir)
            .await
            .with_context(|| {
                format!("Failed to read pending dir: {}", self.pending_dir.display())
            })?;

        let mut paths = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                paths.push(path);
            }
        }

        paths.sort();
        Ok(paths)
    }

    /// Remove a successfully delivered job from the pending queue.
    pub async fn remove(&self, path: &Path) -> Result<()> {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("Failed to remove job: {}", path.display()))
    }

    /// Move a failed/orphaned job to the `dead/` directory with a `.reason` sidecar.
    ///
    /// The reason sidecar is written before the rename for atomicity:
    /// if the process crashes between the two, the file stays in `pending/`
    /// and will be re-evaluated on the next restart.
    pub async fn move_to_dead(&self, path: &Path, reason: &str) -> Result<()> {
        let filename = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Invalid path: no filename"))?;
        let dead_path = self.dead_dir.join(filename);
        let reason_path = self.dead_dir.join(format!(
            "{}.reason",
            filename.to_string_lossy().trim_end_matches(".json")
        ));

        // Write reason sidecar first.
        tokio::fs::write(&reason_path, reason.as_bytes())
            .await
            .with_context(|| {
                format!("Failed to write reason sidecar: {}", reason_path.display())
            })?;

        // Move job to dead/.
        tokio::fs::rename(path, &dead_path).await.with_context(|| {
            format!(
                "Failed to move job to dead/: {} → {}",
                path.display(),
                dead_path.display()
            )
        })?;

        Ok(())
    }

    /// Return the count of pending jobs.
    pub async fn pending_count(&self) -> Result<usize> {
        let paths = self.list_pending().await?;
        Ok(paths.len())
    }

    /// Read and deserialize a job from a path.
    pub async fn read_job(&self, path: &Path) -> Result<DeliveryJob> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read job file: {}", path.display()))?;
        let job: DeliveryJob = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize job: {}", path.display()))?;
        Ok(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_adapters::{ChatBinding, PlatformType};
    use tempfile::tempdir;

    fn make_job(run_id: &str) -> DeliveryJob {
        DeliveryJob {
            schema: "todos".to_string(),
            value: serde_json::json!({"todos": []}),
            run_id: run_id.to_string(),
            source_chat_binding: ChatBinding {
                platform: PlatformType::Telegram,
                chat_id: "12345".to_string(),
                thread_ts: None,
            },
        }
    }

    #[tokio::test]
    async fn enqueue_places_file_in_pending() {
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        let job = make_job("01HPPPPPPPPPPPPPPPPPPPPPPP");
        let path = q.enqueue(&job).await.unwrap();
        assert!(path.exists());
        assert!(path.starts_with(dir.path().join("pending")));
    }

    #[tokio::test]
    async fn enqueue_is_atomic() {
        // A crash after write but before rename would leave a file in tmp/, not pending/.
        // We test the happy path: after a successful enqueue, file is in pending/ not tmp/.
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        let job = make_job("01HPPPPPPPPPPPPPPPPPPPPPPP");
        let pending_path = q.enqueue(&job).await.unwrap();

        assert!(pending_path.exists(), "file should be in pending/");
        let tmp_path = dir
            .path()
            .join("tmp")
            .join("01HPPPPPPPPPPPPPPPPPPPPPPP.json");
        assert!(
            !tmp_path.exists(),
            "file should NOT be in tmp/ after successful enqueue"
        );
    }

    #[tokio::test]
    async fn list_pending_ordering() {
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();

        // Enqueue jobs with ULID-like IDs that sort lexicographically.
        // In real usage these would be real ULIDs; here we use fixed strings
        // that have the correct lexicographic ordering.
        let ids = [
            "01HAAAAAAAAAAAAAAAAAAAAAA1",
            "01HAAAAAAAAAAAAAAAAAAAAAA2",
            "01HAAAAAAAAAAAAAAAAAAAAAA3",
        ];
        for id in &ids {
            q.enqueue(&make_job(id)).await.unwrap();
        }

        let paths = q.list_pending().await.unwrap();
        assert_eq!(paths.len(), 3);

        // Should be sorted by filename (ULID = creation order).
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "list_pending should be sorted by filename");
    }

    #[tokio::test]
    async fn pending_count_matches_list_pending_len() {
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(q.pending_count().await.unwrap(), 0);

        q.enqueue(&make_job("01HAAAAAAAAAAAAAAAAAAAAAA1"))
            .await
            .unwrap();
        assert_eq!(q.pending_count().await.unwrap(), 1);

        q.enqueue(&make_job("01HAAAAAAAAAAAAAAAAAAAAAA2"))
            .await
            .unwrap();
        assert_eq!(q.pending_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn remove_deletes_pending_file() {
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        let path = q
            .enqueue(&make_job("01HAAAAAAAAAAAAAAAAAAAAAA1"))
            .await
            .unwrap();
        assert!(path.exists());
        q.remove(&path).await.unwrap();
        assert!(!path.exists());
    }

    /// Queue job files contain structured-output bodies that may include
    /// information derived from the user's code/context. On Unix, the file
    /// must be 0o600 (owner-only) so other users on a shared host cannot
    /// read it.  If the `set_permissions` call is ever removed or broken,
    /// this test catches it.
    #[cfg(unix)]
    #[tokio::test]
    async fn enqueued_file_has_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        let path = q
            .enqueue(&make_job("01HTESTPERMS0001TESTPERMS0"))
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "queue file must be 0o600 (owner-only), got {:#o}",
            mode & 0o777
        );
    }

    /// Queue subdirectories (tmp/, pending/, dead/) should also be 0o700 so a
    /// casual `ls` from another user cannot enumerate pending run_ids.
    #[cfg(unix)]
    #[tokio::test]
    async fn queue_subdirs_have_mode_0o700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let _q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        for sub in ["tmp", "pending", "dead"] {
            let path = dir.path().join(sub);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "queue subdir {} must be 0o700, got {:#o}",
                sub,
                mode & 0o777
            );
        }
    }

    #[tokio::test]
    async fn move_to_dead_creates_sidecar_and_removes_from_pending() {
        let dir = tempdir().unwrap();
        let q = DeliveryQueue::new(dir.path().to_path_buf()).unwrap();
        let path = q
            .enqueue(&make_job("01HAAAAAAAAAAAAAAAAAAAAAA1"))
            .await
            .unwrap();
        assert!(path.exists());

        q.move_to_dead(&path, "schema_removed").await.unwrap();

        assert!(!path.exists(), "job should be removed from pending");
        let dead_path = dir
            .path()
            .join("dead")
            .join("01HAAAAAAAAAAAAAAAAAAAAAA1.json");
        assert!(dead_path.exists(), "job should be in dead/");
        let reason_path = dir
            .path()
            .join("dead")
            .join("01HAAAAAAAAAAAAAAAAAAAAAA1.reason");
        assert!(reason_path.exists(), "reason sidecar should exist");
        let reason = std::fs::read_to_string(reason_path).unwrap();
        assert_eq!(reason, "schema_removed");
    }

    /// On Linux with bind-mounts this test verifies the cross-device check.
    /// On macOS (and in typical CI), this test is ignored because creating
    /// cross-device configurations without root is not straightforward.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires root/bind-mount to simulate cross-device"]
    fn queue_new_fails_on_cross_device() {
        // This test would require mounting a tmpfs at a subdirectory, which
        // needs CAP_SYS_ADMIN. It is documented here for reference and manual
        // verification. The same-device check is covered by code review and
        // the integration test suite.
    }

    /// macOS version: the check runs but usually succeeds (same device).
    /// Documented so the cross-device path is auditable.
    #[test]
    #[cfg(target_os = "macos")]
    fn queue_new_succeeds_on_local_fs() {
        let dir = tempdir().unwrap();
        // On macOS, tmp/ and pending/ will be on the same device.
        let q = DeliveryQueue::new(dir.path().to_path_buf());
        assert!(q.is_ok(), "Should succeed on local fs: {:?}", q.err());
    }
}
