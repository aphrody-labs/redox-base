pub mod config;
pub mod macros;
pub mod report;
pub mod runner;
mod timing;

pub use config::BaseConfig;
pub use report::{CaseResult, Group, Outcome, SuiteReport};
pub use runner::{TestCase, concurrency, edge, func, perf, run_suite};
pub use timing::{ParallelResult, time_threads, time_workers};
