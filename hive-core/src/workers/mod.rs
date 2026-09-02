//! Worker pool — manages SSH connections to worker machines and task delegation.

use hive_common::{WorkerInfo, WorkerStatus};

/// Pool of worker machines for task delegation.
pub struct WorkerPool {
    /// Worker definitions loaded from config.
    pub workers: Vec<WorkerNode>,
}

/// A worker node with connection state.
pub struct WorkerNode {
    /// Static worker info from config.
    pub info: WorkerInfo,
    /// Current status.
    pub status: WorkerStatus,
    /// Number of active tasks on this worker.
    pub active_tasks: std::sync::atomic::AtomicUsize,
}

impl WorkerPool {
    /// Create a new worker pool from worker configurations.
    pub fn new(workers: Vec<WorkerInfo>) -> Self {
        let nodes = workers
            .into_iter()
            .map(|info| WorkerNode {
                info,
                status: WorkerStatus::Offline,
                active_tasks: std::sync::atomic::AtomicUsize::new(0),
            })
            .collect();

        Self { workers: nodes }
    }

    /// Select the least-loaded online worker.
    pub fn select_worker(&self) -> Option<&WorkerNode> {
        self.workers
            .iter()
            .filter(|w| w.status == WorkerStatus::Online)
            .min_by_key(|w| w.active_tasks.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Get the number of online workers.
    pub fn online_count(&self) -> usize {
        self.workers
            .iter()
            .filter(|w| w.status == WorkerStatus::Online)
            .count()
    }
}
