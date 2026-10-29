use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct BaseConfig {
    #[arg(long = "run-id", default_value_t = default_run_id())]
    pub run_id: String,

    #[arg(long = "perf-only", conflicts_with = "skip_perf")]
    pub perf_only: bool,

    #[arg(long = "skip-perf")]
    pub skip_perf: bool,

    /// Filter test cases by substring
    #[arg(long)]
    pub filter: Option<String>,

    /// Concurrent worker threads
    #[arg(long, default_value_t = 4, value_name = "N")]
    pub threads: usize,

    /// Operations per worker
    #[arg(long, default_value_t = 64, value_name = "N")]
    pub iterations: usize,

    /// Throughput block size (e.g. 64k)
    #[arg(long = "io-block", default_value = "64k", value_parser = parse_size, value_name = "SIZE")]
    pub io_block_size: usize,

    /// Sequential throughput total bytes (e.g. 4m)
    #[arg(long = "io-total", default_value = "4m", value_parser = parse_size, value_name = "SIZE")]
    pub io_total_bytes: usize,

    /// Samples per concurrency_gain op
    #[arg(long = "perf-trials", default_value_t = 3, value_name = "N")]
    pub perf_trials: usize,

    /// List available tests and exit
    #[arg(long = "list")]
    pub list_only: bool,
}

impl BaseConfig {
    pub fn run_non_perf(&self) -> bool {
        !self.perf_only
    }

    pub fn run_perf(&self) -> bool {
        !self.skip_perf
    }
}

pub fn default_run_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{secs}.{pid}")
}

pub fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("k") {
        (n, 1024usize)
    } else if let Some(n) = s.strip_suffix("m") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("g") {
        (n, 1024 * 1024 * 1024)
    } else {
        (s.as_str(), 1)
    };
    num.parse::<usize>()
        .map(|n| n.saturating_mul(mult))
        .map_err(|e| format!("invalid size '{s}': {e}"))
}
