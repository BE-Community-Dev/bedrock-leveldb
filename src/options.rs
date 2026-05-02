/// Compression used when this crate writes its own custom `BWLDB...` tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompressionPolicy {
    /// Store custom table payloads uncompressed.
    None,
    /// Compress custom table payloads with Snappy.
    Snappy,
    /// Compress custom table payloads with zlib.
    #[default]
    Zlib,
}

/// Options used when opening a database directory.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Open without performing writes, initialization, repair, or flushes.
    pub read_only: bool,
    /// Create the database directory and initial custom manifest when missing.
    pub create_if_missing: bool,
    /// Fail if the target directory already contains files.
    pub error_if_exists: bool,
    /// Verify checksums while replaying logs and reading table blocks by default.
    pub paranoid_checks: bool,
    /// Compression used for tables written by this crate.
    pub compression_policy: CompressionPolicy,
    /// Maximum decoded native table block cache size, in bytes.
    pub cache_size: usize,
    /// Approximate overlay size that triggers a flush to a custom table.
    pub write_buffer_size: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            create_if_missing: true,
            error_if_exists: false,
            paranoid_checks: true,
            compression_policy: CompressionPolicy::Zlib,
            cache_size: 64 * 1024 * 1024,
            write_buffer_size: 4 * 1024 * 1024,
        }
    }
}

use crate::error::{LevelDbError, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Per-read behavior for point lookups and scans.
#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// Checksum behavior for this read.
    pub checksum: ChecksumMode,
    /// Whether the native decoded block cache may be used.
    pub cache_policy: CachePolicy,
    /// Worker selection for parallel table scans.
    pub threading: ThreadingOptions,
    /// Sequential or table-parallel scan execution.
    pub scan_mode: ScanMode,
    /// Bounded scan pipeline behavior for parallel scans and progress cadence.
    pub pipeline: ScanPipelineOptions,
    /// Optional cooperative cancellation flag checked during scans.
    pub cancel: Option<ScanCancelFlag>,
    /// Optional progress callback emitted during scans.
    pub progress: Option<ScanProgressSink>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            checksum: ChecksumMode::Inherit,
            cache_policy: CachePolicy::Use,
            threading: ThreadingOptions::Auto,
            scan_mode: ScanMode::Sequential,
            pipeline: ScanPipelineOptions::default(),
            cancel: None,
            progress: None,
        }
    }
}

/// Bounded pipeline policy used by table scans.
///
/// A zero value chooses an automatic default based on worker count and table
/// count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanPipelineOptions {
    /// Maximum number of queued scan messages before worker threads apply
    /// backpressure.
    pub queue_depth: usize,
    /// Number of table files assigned to one Rayon work item.
    pub table_batch_size: usize,
    /// Emit progress after this many visited records.
    pub progress_interval: usize,
}

/// Options used when writing to the overlay and WAL.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    /// Call `File::sync_data` after appending the write batch to the log.
    pub sync: bool,
}

/// How many worker threads a table-parallel scan may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThreadingOptions {
    /// Use available parallelism, capped by the number of table files.
    #[default]
    Auto,
    /// Use an explicit worker count in `1..=512`.
    Fixed(usize),
    /// Force one worker.
    Single,
}

/// Scan execution mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScanMode {
    /// Visit table files on the calling thread in manifest order.
    #[default]
    Sequential,
    /// Partition table files across bounded workers.
    ParallelTables,
}

/// Upper bound for explicit scan worker counts.
pub const MAX_LEVELDB_THREADS: usize = 512;

/// Checksum behavior for a read operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChecksumMode {
    /// Follow `OpenOptions::paranoid_checks`.
    #[default]
    Inherit,
    /// Verify checksums for this read.
    Verify,
    /// Skip checksum verification for this read.
    Skip,
}

/// Cache behavior for a read operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CachePolicy {
    /// Use the database's native block cache.
    #[default]
    Use,
    /// Bypass the native block cache for this read.
    Bypass,
}

/// Visitor result used by scan callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitorControl {
    /// Continue the scan.
    Continue,
    /// Stop the scan without treating it as an error.
    Stop,
}

/// Aggregate information returned after a scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Number of visible records visited.
    pub visited: usize,
    /// Sum of visited value lengths in bytes.
    pub bytes_read: usize,
    /// Whether the visitor stopped the scan.
    pub stopped: bool,
    /// Number of table files that were opened and scanned.
    pub tables_scanned: usize,
    /// Number of worker threads used by the scan.
    pub worker_threads: usize,
    /// Milliseconds workers spent waiting for bounded scan queues.
    pub queue_wait_ms: u128,
    /// Number of cooperative cancellation checks performed by the scan.
    pub cancel_checks: usize,
}

impl ScanOutcome {
    /// Creates an empty scan outcome.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            visited: 0,
            bytes_read: 0,
            stopped: false,
            tables_scanned: 0,
            worker_threads: 0,
            queue_wait_ms: 0,
            cancel_checks: 0,
        }
    }

    /// Adds one visited record with a value length in bytes.
    pub fn record(&mut self, value_len: usize) {
        self.visited = self.visited.saturating_add(1);
        self.bytes_read = self.bytes_read.saturating_add(value_len);
    }

    /// Merges another scan outcome into this one.
    pub fn merge(&mut self, other: Self) {
        self.visited = self.visited.saturating_add(other.visited);
        self.bytes_read = self.bytes_read.saturating_add(other.bytes_read);
        self.stopped |= other.stopped;
        self.tables_scanned = self.tables_scanned.saturating_add(other.tables_scanned);
        self.worker_threads = self.worker_threads.max(other.worker_threads);
        self.queue_wait_ms = self.queue_wait_ms.saturating_add(other.queue_wait_ms);
        self.cancel_checks = self.cancel_checks.saturating_add(other.cancel_checks);
    }
}

/// Shared cooperative cancellation flag for long scans.
#[derive(Debug, Clone, Default)]
pub struct ScanCancelFlag(Arc<AtomicBool>);

impl ScanCancelFlag {
    /// Creates a new non-cancelled flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the flag as cancelled.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Wraps a shared atomic flag supplied by the caller.
    #[must_use]
    pub fn from_shared(cancelled: Arc<AtomicBool>) -> Self {
        Self(cancelled)
    }

    /// Returns whether the flag has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl ScanPipelineOptions {
    /// Resolves the bounded queue depth for a scan.
    #[must_use]
    pub fn resolve_queue_depth(self, workers: usize, tables: usize) -> usize {
        self.queue_depth
            .max(if self.queue_depth == 0 {
                workers.max(1).saturating_mul(256).max(tables.max(1))
            } else {
                1
            })
            .max(1)
    }

    /// Resolves the table batch size for one Rayon task.
    #[must_use]
    pub fn resolve_table_batch_size(self, workers: usize, tables: usize) -> usize {
        self.table_batch_size
            .max(if self.table_batch_size == 0 {
                tables.div_ceil(workers.max(1).saturating_mul(2)).max(1)
            } else {
                1
            })
            .max(1)
    }

    /// Resolves the progress emission interval.
    #[must_use]
    pub fn resolve_progress_interval(self) -> usize {
        self.progress_interval
            .max(if self.progress_interval == 0 { 8192 } else { 1 })
    }
}

/// Callback sink for scan progress.
#[derive(Clone)]
pub struct ScanProgressSink {
    inner: Arc<dyn Fn(ScanProgress) + Send + Sync>,
}

impl std::fmt::Debug for ScanProgressSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScanProgressSink")
            .finish_non_exhaustive()
    }
}

impl ScanProgressSink {
    /// Creates a progress sink from a callback.
    #[must_use]
    pub fn new(callback: impl Fn(ScanProgress) + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(callback),
        }
    }

    /// Emits one progress sample.
    pub fn emit(&self, progress: ScanProgress) {
        (self.inner)(progress);
    }
}

/// Progress sample emitted during scans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanProgress {
    /// Number of visible records visited so far.
    pub visited: usize,
    /// Sum of visited value lengths in bytes so far.
    pub bytes_read: usize,
}

impl ThreadingOptions {
    /// Resolves this setting to a concrete worker count.
    #[must_use]
    pub fn resolve(self, work_items: usize) -> usize {
        self.resolve_unchecked(work_items)
    }

    /// Resolves this setting without returning validation errors.
    #[must_use]
    pub fn resolve_unchecked(self, work_items: usize) -> usize {
        match self {
            Self::Single => 1,
            Self::Fixed(threads) => threads.clamp(1, MAX_LEVELDB_THREADS),
            Self::Auto => std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .min(work_items.max(1)),
        }
    }

    /// Resolves this setting and rejects invalid fixed worker counts.
    ///
    /// # Errors
    ///
    /// Returns [`LevelDbError::InvalidArgument`] when `Fixed(0)` or a value
    /// above 512 is requested.
    pub fn resolve_checked(self, work_items: usize) -> Result<usize> {
        match self {
            Self::Fixed(0) => Err(LevelDbError::invalid_argument(
                "thread count must be in 1..=512",
            )),
            Self::Fixed(threads) if threads > MAX_LEVELDB_THREADS => Err(
                LevelDbError::invalid_argument("thread count must be in 1..=512"),
            ),
            _ => Ok(self.resolve_unchecked(work_items)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threading_validates_fixed_range_and_auto_is_not_capped_to_eight() {
        let expected_auto = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(10_000);
        assert_eq!(
            ThreadingOptions::Auto
                .resolve_checked(10_000)
                .expect("auto threads"),
            expected_auto
        );
        assert_eq!(
            ThreadingOptions::Fixed(MAX_LEVELDB_THREADS)
                .resolve_checked(10_000)
                .expect("max fixed threads"),
            MAX_LEVELDB_THREADS
        );
        assert!(ThreadingOptions::Fixed(0).resolve_checked(10).is_err());
        assert!(
            ThreadingOptions::Fixed(MAX_LEVELDB_THREADS + 1)
                .resolve_checked(10)
                .is_err()
        );
    }

    #[test]
    fn scan_pipeline_options_resolve_automatic_bounds() {
        let options = ScanPipelineOptions::default();

        assert!(options.resolve_queue_depth(4, 128) >= 1);
        assert!(options.resolve_table_batch_size(4, 128) >= 1);
        assert_eq!(options.resolve_progress_interval(), 8192);

        let explicit = ScanPipelineOptions {
            queue_depth: 7,
            table_batch_size: 3,
            progress_interval: 11,
        };
        assert_eq!(explicit.resolve_queue_depth(4, 128), 7);
        assert_eq!(explicit.resolve_table_batch_size(4, 128), 3);
        assert_eq!(explicit.resolve_progress_interval(), 11);
    }
}
