use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Runs `n` workers in scoped threads, synchronized with start and end barriers.
pub fn time_workers<F>(n: usize, worker: F) -> Duration
where
    F: Fn(usize, &Barrier, &Barrier) + Sync,
{
    let n = n.max(1);
    let start_b = Barrier::new(n + 1);
    let end_b = Barrier::new(n + 1);
    thread::scope(|scope| {
        for t in 0..n {
            let worker = &worker;
            let start_b = &start_b;
            let end_b = &end_b;
            scope.spawn(move || worker(t, start_b, end_b));
        }
        start_b.wait();
        let t0 = Instant::now();
        end_b.wait();
        t0.elapsed()
    })
}

#[derive(Debug, Clone, Copy)]
pub struct ParallelResult {
    pub elapsed: Duration,
    pub errors: usize,
}

/// Runs `threads` worker threads for `iters` operations each, tracking errors.
pub fn time_threads<F>(threads: usize, iters: usize, f: F) -> ParallelResult
where
    F: Fn(usize, usize) -> std::io::Result<()> + Sync,
{
    let errors = AtomicUsize::new(0);
    let elapsed = time_workers(threads, |t, start_b, end_b| {
        start_b.wait();
        for i in 0..iters {
            if let Err(e) = f(t, i) {
                eprintln!("    worker {t}/{i}: {e}");
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        end_b.wait();
    });
    ParallelResult {
        elapsed,
        errors: errors.load(Ordering::Relaxed),
    }
}
