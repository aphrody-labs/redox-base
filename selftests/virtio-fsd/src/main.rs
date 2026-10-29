#[macro_use]
pub mod common;
mod config;
mod fsutil;
mod perf;
mod tests;
use std::process::ExitCode;

pub use common::{Group, Outcome, concurrency, edge, func, perf};
use config::Config;

use crate::common::run_suite;

#[derive(Debug)]
pub struct TestCtx {
    pub cfg: Config,
}

impl TestCtx {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.cfg.root
    }

    pub fn fixtures(&self) -> std::path::PathBuf {
        self.cfg.fixtures_dir()
    }

    pub fn work(&self, name: &str) -> std::path::PathBuf {
        self.cfg.work_path(name)
    }

    pub fn require_fixtures(&self) -> Result<(), Outcome> {
        let dir = self.fixtures();
        if fsutil::is_dir(&dir) {
            Ok(())
        } else {
            Err(Outcome::Skip(format!(
                "fixtures missing at {} - run scripts/gen-fixtures.sh on the host test root",
                dir.display()
            )))
        }
    }
}

pub type TestCase = crate::common::TestCase<TestCtx>;

fn main() -> ExitCode {
    let cfg = Config::from_args(std::env::args());

    println!("virtio-fsd-selftest {}", env!("CARGO_PKG_VERSION"));
    println!("  share_tag: {}", cfg.share_tag);
    println!("  root:      {}", cfg.root.display());
    println!("  run_id:    {}", cfg.run_id);
    println!(
        "  groups:    functional/edge/concurrency={}  perf={}",
        cfg.run_non_perf(),
        cfg.run_perf()
    );
    if let Some(f) = &cfg.filter {
        println!("  filter:    {f}");
    }
    if cfg.run_perf() {
        println!(
            "  perf:      threads={} iterations={} io_block={} io_total={} trials={}",
            cfg.threads, cfg.iterations, cfg.io_block_size, cfg.io_total_bytes, cfg.perf_trials
        );
    }
    println!();

    if !cfg.list_only {
        if let Err(e) = fsutil::open_dir(&cfg.root) {
            eprintln!(
                "error: cannot open test root {}: {e}\n\
                 Is virtio-fsd running and the path present on the mount?",
                cfg.root.display()
            );
            return ExitCode::from(2);
        }
    }

    let mut cases = Vec::new();
    cases.extend(tests::all_cases());
    cases.extend(perf::all_cases());

    let ctx = TestCtx::new(cfg.clone());

    if cfg.list_only {
        println!("available cases:");
        let _ = run_suite(&cfg, &ctx, &cases);
        return ExitCode::SUCCESS;
    }

    println!("# selftest: virtio-fsd");
    let report = run_suite(&cfg, &ctx, &cases);
    report.print_summary();

    let prefix = cfg.work_name_prefix();
    let (removed, errs) = fsutil::cleanup_prefixed(&cfg.root, &prefix);

    println!("cleanup: removed {removed} leftover `{prefix}*`");
    for (p, e) in &errs {
        println!("  cleanup fail {}: {e}", p.display());
    }

    report.print_group_breakdown();

    if report.failed() > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
