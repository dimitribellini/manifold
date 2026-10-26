//! Prioritized File Transfer Engine, Token-Bucket Bandwidth Throttler, and
//! Conflict Resolution Engine (`doc/image4.png`) for Project Manifold.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use manifold_vfs::{FileMetadata, FileSystemBackend, VfsError};
use serde::{Deserialize, Serialize};

/// Uniform default chunk size for remote transfers (4 MB, within the 4–8 MB range specified in `Section 3.4` of `BRIEF.md`).
pub const DEFAULT_TRANSFER_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Priority lane for the transfer queue (`Section 3.4` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TransferPriority {
    Background = 0,
    Foreground = 1,
}

/// Interactive and automated conflict resolution actions matching the vertical button stack in `doc/image4.png`
/// and `Section 3.4` of `BRIEF.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Primary blue button `[ Replace ]` (`Overwrite`).
    Replace,
    /// Row button `Replace Older` (`Overwrite if Newer`).
    ReplaceOlder,
    /// `Overwrite if Size Different` (`Section 3.4` of `BRIEF.md`).
    ReplaceIfSizeDifferent,
    /// Row button `Keep Both` (auto-renames incoming file e.g. `"Text 2.txt"`).
    KeepBoth,
    /// Row button `Skip`.
    Skip,
    /// Secondary gray button `[ Stop ]` (cancels remaining batch).
    Stop,
    /// Prompt the user via the Conflict Resolution Modal (`doc/image4.png`).
    Ask,
}

/// Side-by-side badge descriptor (`New` vs `Existing`) rendered inside the Conflict Resolution Modal (`doc/image4.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSideBadge {
    pub badge_label: String,
    pub badge_accent_hex: String,
    pub formatted_modified: String,
    pub formatted_size: String,
    pub size_bytes: u64,
    pub modified: Option<SystemTime>,
}

/// Full view-model for the File Conflict Resolution Modal (`doc/image4.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictModalDescriptor {
    pub filename: String,
    pub headline: String,
    pub subheadline: String,
    pub new_item: ConflictSideBadge,
    pub existing_item: ConflictSideBadge,
    pub actions: Vec<ConflictResolution>,
    pub apply_to_all: bool,
}

impl ConflictModalDescriptor {
    /// Constructs the exact modal view-model shown in `doc/image4.png`.
    pub fn from_collision(
        source: &FileMetadata,
        existing: &FileMetadata,
        formatted_new_date: impl Into<String>,
        formatted_existing_date: impl Into<String>,
    ) -> Self {
        let new_size = source.size.unwrap_or(0);
        let existing_size = existing.size.unwrap_or(0);
        let subheadline = "Do you want to replace it with the one you\u{2019}re moving?";
        Self {
            filename: source.name.clone(),
            headline: format!(
                "An item named \"{}\" already exists in this location.",
                source.name
            ),
            subheadline: subheadline.to_string(),
            new_item: ConflictSideBadge {
                badge_label: "New".to_string(),
                badge_accent_hex: "#0A84FF".to_string(),
                formatted_modified: formatted_new_date.into(),
                formatted_size: manifold_vfs::format_bytes_comma(new_size),
                size_bytes: new_size,
                modified: source.modified,
            },
            existing_item: ConflictSideBadge {
                badge_label: "Existing".to_string(),
                badge_accent_hex: "#636366".to_string(),
                formatted_modified: formatted_existing_date.into(),
                formatted_size: manifold_vfs::format_bytes_comma(existing_size),
                size_bytes: existing_size,
                modified: existing.modified,
            },
            actions: vec![
                ConflictResolution::Replace,
                ConflictResolution::ReplaceOlder,
                ConflictResolution::KeepBoth,
                ConflictResolution::Skip,
                ConflictResolution::Stop,
            ],
            apply_to_all: false,
        }
    }
}

/// Token-bucket bandwidth throttler (`Section 3.4` of `BRIEF.md`).
pub struct TokenBucketLimiter {
    rate_bytes_per_sec: Option<u64>,
    available_tokens: f64,
    last_refill: Instant,
}

impl TokenBucketLimiter {
    /// Creates a new limiter (`None` = unlimited throughput).
    pub fn new(rate_bytes_per_sec: Option<u64>) -> Self {
        Self {
            rate_bytes_per_sec,
            available_tokens: rate_bytes_per_sec.unwrap_or(0) as f64,
            last_refill: Instant::now(),
        }
    }

    pub fn set_rate(&mut self, rate_bytes_per_sec: Option<u64>) {
        self.rate_bytes_per_sec = rate_bytes_per_sec;
        self.available_tokens = rate_bytes_per_sec.unwrap_or(0) as f64;
        self.last_refill = Instant::now();
    }

    /// Calculates the required delay to consume `bytes` under the configured token-bucket rate.
    pub fn consume(&mut self, bytes: usize) -> Duration {
        let Some(rate) = self.rate_bytes_per_sec else {
            return Duration::ZERO;
        };
        if rate == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        let max_tokens = rate as f64;
        self.available_tokens = (self.available_tokens + elapsed * max_tokens).min(max_tokens);

        let needed = bytes as f64;
        if self.available_tokens >= needed {
            self.available_tokens -= needed;
            Duration::ZERO
        } else {
            let deficit = needed - self.available_tokens;
            self.available_tokens = 0.0;
            Duration::from_secs_f64(deficit / (rate as f64))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferStatus {
    Queued,
    InProgress,
    Paused,
    WaitingForConflictResolution,
    Completed,
    Skipped,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferTask {
    pub id: String,
    pub priority: TransferPriority,
    pub source_path: String,
    pub destination_path: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub chunk_size: usize,
    pub max_retries: u32,
    pub status: TransferStatus,
    pub error_message: Option<String>,
}

impl TransferTask {
    pub fn progress_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return if self.status == TransferStatus::Completed {
                1.0
            } else {
                0.0
            };
        }
        (self.transferred_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }
}

/// Manages prioritized transfer tasks, 4–8 MB chunked I/O, resume offsets, and conflict resolution.
pub struct TransferEngine {
    foreground_queue: Arc<Mutex<VecDeque<TransferTask>>>,
    background_queue: Arc<Mutex<VecDeque<TransferTask>>>,
    completed_tasks: Arc<Mutex<Vec<TransferTask>>>,
    limiter: Arc<Mutex<TokenBucketLimiter>>,
    sticky_conflict_policy: Arc<Mutex<Option<ConflictResolution>>>,
}

impl Default for TransferEngine {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TransferEngine {
    pub fn new(bandwidth_limit_bps: Option<u64>) -> Self {
        Self {
            foreground_queue: Arc::new(Mutex::new(VecDeque::new())),
            background_queue: Arc::new(Mutex::new(VecDeque::new())),
            completed_tasks: Arc::new(Mutex::new(Vec::new())),
            limiter: Arc::new(Mutex::new(TokenBucketLimiter::new(bandwidth_limit_bps))),
            sticky_conflict_policy: Arc::new(Mutex::new(None)),
        }
    }

    pub fn enqueue(&self, task: TransferTask) {
        match task.priority {
            TransferPriority::Foreground => {
                self.foreground_queue.lock().unwrap().push_back(task);
            }
            TransferPriority::Background => {
                self.background_queue.lock().unwrap().push_back(task);
            }
        }
    }

    /// Pops the next task respecting priority lanes (`Foreground` always preempts `Background`).
    pub fn pop_next_task(&self) -> Option<TransferTask> {
        if let Some(fg) = self.foreground_queue.lock().unwrap().pop_front() {
            return Some(fg);
        }
        self.background_queue.lock().unwrap().pop_front()
    }

    /// Generates a non-colliding `Keep Both` path (e.g., `/dir/Text.txt` -> `/dir/Text 2.txt`).
    pub fn generate_keep_both_path(dst_vfs: &dyn FileSystemBackend, original_dst: &str) -> String {
        let path = Path::new(original_dst);
        let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("item");
        let ext = path.extension().and_then(|e| e.to_str());

        for counter in 2..10_000 {
            let candidate_name = match ext {
                Some(e) => format!("{stem} {counter}.{e}"),
                None => format!("{stem} {counter}"),
            };
            let candidate = if parent.is_empty() || parent == "/" {
                format!("/{candidate_name}")
            } else {
                format!("{parent}/{candidate_name}")
            };
            if dst_vfs.metadata(&candidate).is_err() {
                return candidate;
            }
        }
        format!("{original_dst}.copy")
    }

    /// Executes a single task across `src_vfs` and `dst_vfs` using chunked I/O and conflict resolution.
    pub fn execute_task(
        &self,
        mut task: TransferTask,
        src_vfs: &dyn FileSystemBackend,
        dst_vfs: &dyn FileSystemBackend,
        policy: ConflictResolution,
        apply_to_all: bool,
    ) -> Result<TransferTask, VfsError> {
        if apply_to_all && policy != ConflictResolution::Ask {
            *self.sticky_conflict_policy.lock().unwrap() = Some(policy);
        }
        let sticky = *self.sticky_conflict_policy.lock().unwrap();
        let effective_policy = sticky.unwrap_or(policy);

        let src_meta = src_vfs.metadata(&task.source_path)?;
        task.total_bytes = src_meta.size.unwrap_or(0);

        let mut target_path = task.destination_path.clone();

        // Check if destination already exists to trigger the Conflict Resolution Engine
        if let Ok(existing_meta) = dst_vfs.metadata(&target_path) {
            match effective_policy {
                ConflictResolution::Ask => {
                    task.status = TransferStatus::WaitingForConflictResolution;
                    return Ok(task);
                }
                ConflictResolution::Skip => {
                    task.status = TransferStatus::Skipped;
                    self.completed_tasks.lock().unwrap().push(task.clone());
                    return Ok(task);
                }
                ConflictResolution::Stop => {
                    self.foreground_queue.lock().unwrap().clear();
                    self.background_queue.lock().unwrap().clear();
                    task.status = TransferStatus::Cancelled;
                    self.completed_tasks.lock().unwrap().push(task.clone());
                    return Ok(task);
                }
                ConflictResolution::ReplaceOlder => {
                    let src_newer = match (src_meta.modified, existing_meta.modified) {
                        (Some(s), Some(e)) => s > e,
                        _ => true,
                    };
                    if !src_newer {
                        task.status = TransferStatus::Skipped;
                        self.completed_tasks.lock().unwrap().push(task.clone());
                        return Ok(task);
                    }
                }
                ConflictResolution::ReplaceIfSizeDifferent => {
                    if src_meta.size == existing_meta.size {
                        task.status = TransferStatus::Skipped;
                        self.completed_tasks.lock().unwrap().push(task.clone());
                        return Ok(task);
                    }
                }
                ConflictResolution::KeepBoth => {
                    target_path = Self::generate_keep_both_path(dst_vfs, &target_path);
                    task.destination_path = target_path.clone();
                }
                ConflictResolution::Replace => {}
            }
        }

        task.status = TransferStatus::InProgress;
        let chunk_size = task.chunk_size.max(1024);
        let mut offset = task.transferred_bytes;
        let mut assembled = if offset > 0 {
            dst_vfs.read_bytes(&target_path).unwrap_or_default()
        } else {
            Vec::with_capacity(task.total_bytes as usize)
        };

        while offset < task.total_bytes {
            let remaining = (task.total_bytes - offset) as usize;
            let to_read = remaining.min(chunk_size);

            let mut attempt = 0;
            let chunk = loop {
                match src_vfs.read_range(&task.source_path, offset, to_read) {
                    Ok(bytes) => break bytes,
                    Err(err) => {
                        attempt += 1;
                        if attempt > task.max_retries {
                            task.status = TransferStatus::Failed;
                            task.error_message = Some(err.to_string());
                            return Err(err);
                        }
                    }
                }
            };

            if chunk.is_empty() {
                break;
            }
            let _throttle_delay = self.limiter.lock().unwrap().consume(chunk.len());
            assembled.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
            task.transferred_bytes = offset;
        }

        dst_vfs.write_bytes(&target_path, &assembled)?;
        task.status = TransferStatus::Completed;
        self.completed_tasks.lock().unwrap().push(task.clone());
        Ok(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifold_vfs::{ArchiveBackend, ArchiveFormat};

    #[test]
    fn test_conflict_modal_descriptor_matches_image4() {
        let src_vfs = ArchiveBackend::new("src.zip", ArchiveFormat::Zip);
        let dst_vfs = ArchiveBackend::new("dst.zip", ArchiveFormat::Zip);

        src_vfs.write_bytes("/Text.txt", &[b'A'; 3072]).unwrap();
        dst_vfs.write_bytes("/Text.txt", &[b'B'; 1024]).unwrap();

        let src_meta = src_vfs.metadata("/Text.txt").unwrap();
        let dst_meta = dst_vfs.metadata("/Text.txt").unwrap();

        let modal = ConflictModalDescriptor::from_collision(
            &src_meta,
            &dst_meta,
            "2023.09.04, 20:27",
            "2023.09.04, 20:27",
        );

        assert_eq!(
            modal.headline,
            "An item named \"Text.txt\" already exists in this location."
        );
        assert_eq!(
            modal.subheadline,
            "Do you want to replace it with the one you\u{2019}re moving?"
        );
        assert_eq!(modal.new_item.badge_label, "New");
        assert_eq!(modal.new_item.formatted_size, "3 KB");
        assert_eq!(modal.existing_item.badge_label, "Existing");
        assert_eq!(modal.existing_item.formatted_size, "1 KB");
        assert_eq!(
            modal.actions,
            vec![
                ConflictResolution::Replace,
                ConflictResolution::ReplaceOlder,
                ConflictResolution::KeepBoth,
                ConflictResolution::Skip,
                ConflictResolution::Stop,
            ]
        );
    }

    #[test]
    fn test_priority_lanes_and_keep_both_conflict_resolution() {
        let engine = TransferEngine::new(Some(10 * 1024 * 1024));
        let src_vfs = ArchiveBackend::new("src.zip", ArchiveFormat::Zip);
        let dst_vfs = ArchiveBackend::new("dst.zip", ArchiveFormat::Zip);

        let existing = b"existing content";
        src_vfs.write_bytes("/Text.txt", b"new content").unwrap();
        dst_vfs.write_bytes("/Text.txt", existing).unwrap();

        engine.enqueue(TransferTask {
            id: "bg-1".to_string(),
            priority: TransferPriority::Background,
            source_path: "/Text.txt".to_string(),
            destination_path: "/Text.txt".to_string(),
            total_bytes: 11,
            transferred_bytes: 0,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            max_retries: 3,
            status: TransferStatus::Queued,
            error_message: None,
        });
        engine.enqueue(TransferTask {
            id: "fg-1".to_string(),
            priority: TransferPriority::Foreground,
            source_path: "/Text.txt".to_string(),
            destination_path: "/Text.txt".to_string(),
            total_bytes: 11,
            transferred_bytes: 0,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            max_retries: 3,
            status: TransferStatus::Queued,
            error_message: None,
        });

        // Foreground lane must be popped before Background lane
        let next = engine.pop_next_task().unwrap();
        assert_eq!(next.id, "fg-1");

        // Resolve conflict using KeepBoth ("/Text 2.txt")
        let policy = ConflictResolution::KeepBoth;
        let res = engine.execute_task(next, &src_vfs, &dst_vfs, policy, false);
        let finished = res.unwrap();
        assert_eq!(finished.status, TransferStatus::Completed);
        assert_eq!(finished.destination_path, "/Text 2.txt");
        assert_eq!(dst_vfs.read_bytes("/Text 2.txt").unwrap(), b"new content");
        assert_eq!(dst_vfs.read_bytes("/Text.txt").unwrap(), existing);
    }
}
