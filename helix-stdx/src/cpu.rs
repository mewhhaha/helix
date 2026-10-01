//! A shared pool for CPU work, leaving room for input, rendering and I/O.

use std::sync::LazyLock;

static COUNTS: LazyLock<(usize, usize)> = LazyLock::new(|| {
    let available = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .saturating_sub(1);
    ((available / 2).clamp(1, 4), (available / 4).clamp(1, 2))
});

/// Number of workers shared by indexing, completion and response decoding.
pub fn worker_count() -> usize {
    COUNTS.0
}

/// Limit libraries which own their own pools (Nucleo and ignore's walker).
pub fn auxiliary_worker_count() -> usize {
    COUNTS.1
}

pub fn pool() -> &'static rayon::ThreadPool {
    static POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count())
            .thread_name(|index| format!("helix-cpu-{index}"))
            .build()
            .expect("failed to start CPU workers")
    });
    &POOL
}
