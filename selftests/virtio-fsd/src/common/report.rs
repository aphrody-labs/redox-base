use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Outcome {
    Pass,
    PassNote(String),
    Fail(String),
    Skip(String),
}

impl Outcome {
    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Fail(_))
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass | Outcome::PassNote(_))
    }

    pub fn status(&self) -> &'static str {
        match self {
            Outcome::Pass | Outcome::PassNote(_) => "PASS",
            Outcome::Fail(_) => "FAIL",
            Outcome::Skip(_) => "SKIP",
        }
    }

    pub fn note(&self) -> Option<&str> {
        match self {
            Outcome::PassNote(s) | Outcome::Fail(s) | Outcome::Skip(s) => Some(s),
            Outcome::Pass => None,
        }
    }

    /// Lets a helper that returns an Outcome compose with `check_out!`.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_pass() { Ok(()) } else { Err(self) }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Pass | Outcome::PassNote(_) => write!(f, "PASS"),
            Outcome::Fail(m) => write!(f, "FAIL: {m}"),
            Outcome::Skip(m) => write!(f, "SKIP: {m}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Functional,
    Edge,
    Concurrency,
    Perf,
}

impl Group {
    pub const LABEL_WIDTH: usize = 11;
    pub const NAME_WIDTH: usize = 40;

    pub fn as_str(self) -> &'static str {
        match self {
            Group::Functional => "functional",
            Group::Edge => "edge",
            Group::Concurrency => "concurrency",
            Group::Perf => "perf",
        }
    }
}

#[derive(Debug)]
pub struct CaseResult {
    pub name: &'static str,
    pub group: Group,
    pub outcome: Outcome,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct SuiteReport {
    pub cases: Vec<CaseResult>,
}

impl SuiteReport {
    pub fn passed(&self) -> usize {
        self.cases.iter().filter(|c| c.outcome.is_pass()).count()
    }

    pub fn failed(&self) -> usize {
        self.cases.iter().filter(|c| c.outcome.is_fail()).count()
    }

    pub fn skipped(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c.outcome, Outcome::Skip(_)))
            .count()
    }

    pub fn total_elapsed(&self) -> Duration {
        self.cases.iter().map(|c| c.elapsed).sum()
    }

    pub fn print_summary(&self) {
        println!();
        println!("────────────────────────────────────────────────────────");
        println!(
            "summary: {} passed, {} failed, {} skipped  ({:.3}s)",
            self.passed(),
            self.failed(),
            self.skipped(),
            self.total_elapsed().as_secs_f64()
        );
        if self.failed() > 0 {
            println!("failures:");
            for c in &self.cases {
                if let Outcome::Fail(msg) = &c.outcome {
                    println!(
                        "  [{:<label$}]  {}: {msg}",
                        c.group.as_str(),
                        c.name,
                        label = Group::LABEL_WIDTH
                    );
                }
            }
        }
    }

    pub fn print_group_breakdown(&self) {
        for group in [
            Group::Functional,
            Group::Edge,
            Group::Concurrency,
            Group::Perf,
        ] {
            let subset: Vec<_> = self.cases.iter().filter(|c| c.group == group).collect();
            if subset.is_empty() {
                continue;
            }
            let pass = subset.iter().filter(|c| c.outcome.is_pass()).count();
            let fail = subset.iter().filter(|c| c.outcome.is_fail()).count();
            let skip = subset.len() - pass - fail;
            println!(
                "  {:<label$}  pass={pass}  fail={fail}  skip={skip}",
                group.as_str(),
                label = Group::LABEL_WIDTH
            );
        }
    }
}
