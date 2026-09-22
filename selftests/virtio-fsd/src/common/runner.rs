use std::time::Instant;

use super::config::BaseConfig;
use super::report::{CaseResult, Group, Outcome, SuiteReport};

pub struct TestCase<Ctx> {
    pub name: &'static str,
    pub group: Group,
    pub run: Box<dyn Fn(&Ctx) -> Outcome>,
}

fn case<Ctx>(
    name: &'static str,
    group: Group,
    run: impl Fn(&Ctx) -> Outcome + 'static,
) -> TestCase<Ctx> {
    TestCase {
        name,
        group,
        run: Box::new(run),
    }
}

pub fn func<Ctx>(name: &'static str, run: impl Fn(&Ctx) -> Outcome + 'static) -> TestCase<Ctx> {
    case(name, Group::Functional, run)
}

pub fn edge<Ctx>(name: &'static str, run: impl Fn(&Ctx) -> Outcome + 'static) -> TestCase<Ctx> {
    case(name, Group::Edge, run)
}

pub fn perf<Ctx>(name: &'static str, run: impl Fn(&Ctx) -> Outcome + 'static) -> TestCase<Ctx> {
    case(name, Group::Perf, run)
}

pub fn concurrency<Ctx>(
    name: &'static str,
    run: impl Fn(&Ctx) -> Outcome + 'static,
) -> TestCase<Ctx> {
    case(name, Group::Concurrency, run)
}

fn group_enabled(cfg: &BaseConfig, group: Group) -> bool {
    match group {
        Group::Functional | Group::Edge | Group::Concurrency => cfg.run_non_perf(),
        Group::Perf => cfg.run_perf(),
    }
}

pub fn run_suite<Ctx>(cfg: &BaseConfig, ctx: &Ctx, cases: &[TestCase<Ctx>]) -> SuiteReport {
    let mut report = SuiteReport::default();

    for case in cases {
        if !group_enabled(cfg, case.group) {
            continue;
        }

        if let Some(filter) = &cfg.filter {
            if !case.name.contains(filter) {
                continue;
            }
        }

        if cfg.list_only {
            println!(
                "  [{:<label$}]  {}",
                case.group.as_str(),
                case.name,
                label = Group::LABEL_WIDTH
            );
            continue;
        }

        print!(
            "  [{:<label$}]  {:<name$} ",
            case.group.as_str(),
            case.name,
            label = Group::LABEL_WIDTH,
            name = Group::NAME_WIDTH
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());

        let start = Instant::now();
        // TODO: run each case in its own process
        let outcome = (case.run)(ctx);
        let elapsed = start.elapsed();

        println!("{}  ({:.3}s)", outcome.status(), elapsed.as_secs_f64());
        if let Some(note) = outcome.note() {
            let indent = 2 + 1 + Group::LABEL_WIDTH + 1 + 2;
            for line in note.lines() {
                println!("{:indent$}{line}", "");
            }
        }

        report.cases.push(CaseResult {
            name: case.name,
            group: case.group,
            outcome,
            elapsed,
        });
    }

    report
}
