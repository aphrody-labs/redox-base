use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::common::{ParallelResult, time_threads, time_workers};
use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_ok, check_out, fsutil,
};

pub fn all_cases() -> Vec<TestCase> {
    cases![
        perf perf_seq_write => seq_write,
        perf perf_seq_read => seq_read,
        perf perf_open_close => open_close,
        perf perf_concurrent_readers => concurrent_readers,
        perf perf_concurrent_writers => concurrent_writers,
        perf perf_concurrency_gain => concurrency_gain,
    ]
}

fn seq_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("perf_seq_write.bin");
    let block = ctx.cfg.io_block_size;
    let total = ctx.cfg.io_total_bytes;
    let buf = vec![0xA5u8; block];
    let mut f = check_ok!(fsutil::create_rw(&path), "create");
    let start = Instant::now();
    let mut written = 0usize;
    while written < total {
        let n = (total - written).min(block);
        check_ok!(f.write_all(&buf[..n]), "write");
        written += n;
    }
    let elapsed = start.elapsed();
    check_ok!(f.sync_all(), "fsync");
    Outcome::PassNote(throughput("write", written as u64, elapsed, block))
}

fn seq_read(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("perf_seq_read.bin");
    let block = ctx.cfg.io_block_size;
    let total = ctx.cfg.io_total_bytes;
    {
        let buf = vec![0x5Au8; block];
        let mut f = check_ok!(fsutil::create_rw(&path), "seed");
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(block);
            check_ok!(f.write_all(&buf[..n]), "seed");
            written += n;
        }
        check_ok!(f.sync_all(), "seed fsync");
    }
    let mut f = check_ok!(fsutil::open_ro(&path), "open");
    let mut buf = vec![0u8; block];
    loop {
        let n = check_ok!(f.read(&mut buf), "warmup read");
        if n == 0 {
            break;
        }
    }
    check_ok!(f.seek(SeekFrom::Start(0)), "rewind");
    let start = Instant::now();
    let mut read_total = 0usize;
    loop {
        let n = check_ok!(f.read(&mut buf), "read");
        if n == 0 {
            break;
        }
        read_total += n;
    }
    check!(
        read_total >= total,
        "read {read_total}, expected >= {total}"
    );
    Outcome::PassNote(throughput(
        "read",
        read_total as u64,
        start.elapsed(),
        block,
    ))
}

fn open_close(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("perf_open_close.txt");
    check_ok!(fsutil::write_all_path(&path, b"storm"), "seed");
    let n = ctx.cfg.iterations.saturating_mul(ctx.cfg.threads.max(1));
    let warm = n.min(32);
    for i in 0..warm {
        drop(check_ok!(fsutil::open_ro(&path), "warmup {i}"));
    }
    let start = Instant::now();
    for i in 0..n {
        drop(check_ok!(fsutil::open_ro(&path), "open {i}"));
    }
    let elapsed = start.elapsed();
    let rate = n as f64 / elapsed.as_secs_f64().max(1e-9);
    Outcome::PassNote(format!(
        "open+close  {n} ops in {:.3}s → {rate:.0} ops/s",
        elapsed.as_secs_f64()
    ))
}

fn concurrent_readers(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("perf_cread.bin");
    let block = ctx.cfg.io_block_size.max(1);
    let total = ctx.cfg.io_total_bytes;
    let seed = vec![0x5Au8; block];
    {
        let mut f = check_ok!(fsutil::create_rw(&path), "seed");
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(block);
            check_ok!(f.write_all(&seed[..n]), "seed");
            written += n;
        }
        check_ok!(f.sync_all(), "seed fsync");
    }
    let threads = ctx.cfg.threads.max(1);
    let errors = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let elapsed = time_workers(threads, |t, start_b, end_b| {
        let mut f = match fsutil::open_ro(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("    reader {t} open: {e}");
                errors.fetch_add(1, Ordering::Relaxed);
                start_b.wait();
                end_b.wait();
                return;
            }
        };
        let mut buf = vec![0u8; block];
        if let Err(e) = read_file(
            &mut f,
            &mut buf,
            &seed,
            total,
            &format!("reader {t} warmup"),
        ) {
            eprintln!("    {e}");
            errors.fetch_add(1, Ordering::Relaxed);
        }
        start_b.wait();
        if f.seek(SeekFrom::Start(0)).is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
            end_b.wait();
            return;
        }
        match read_file(&mut f, &mut buf, &seed, total, &format!("reader {t}")) {
            Ok(n) => {
                bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("    {e}");
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        end_b.wait();
    });
    check_eq!(errors.load(Ordering::Relaxed), 0);
    Outcome::PassNote(format!(
        "{}  (same-file held-fd, {threads} threads)",
        throughput("read", bytes.load(Ordering::Relaxed), elapsed, block),
    ))
}

fn concurrent_writers(ctx: &TestCtx) -> Outcome {
    let threads = ctx.cfg.threads.max(1);
    let block = ctx.cfg.io_block_size.max(1);
    let total = ctx.cfg.io_total_bytes;
    let errors = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let paths: Vec<PathBuf> = (0..threads)
        .map(|t| ctx.work(&format!("perf_cwrite_{t}.bin")))
        .collect();
    let elapsed = time_workers(threads, |t, start_b, end_b| {
        let path = &paths[t];
        let mut f = match fsutil::create_rw(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("    writer {t} create: {e}");
                errors.fetch_add(1, Ordering::Relaxed);
                start_b.wait();
                end_b.wait();
                return;
            }
        };
        let buf = vec![(t & 0xff) as u8; block];
        start_b.wait();
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(block);
            if let Err(e) = f.write_all(&buf[..n]) {
                eprintln!("    writer {t}: {e}");
                errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            written += n;
        }
        bytes.fetch_add(written as u64, Ordering::Relaxed);
        end_b.wait();
        if let Err(e) = f.sync_all() {
            eprintln!("    writer {t} fsync: {e}");
            errors.fetch_add(1, Ordering::Relaxed);
        }
    });
    check_eq!(errors.load(Ordering::Relaxed), 0);
    for (t, path) in paths.iter().enumerate() {
        let got = check_ok!(fsutil::read_to_vec(path), "reread {t}");
        let byte = (t & 0xff) as u8;
        check!(
            got.len() == total && got.iter().all(|&b| b == byte),
            "writer {t} contents: len {} want {total}",
            got.len()
        );
    }
    Outcome::PassNote(format!(
        "{}  (distinct files, {threads} threads)",
        throughput("write", bytes.load(Ordering::Relaxed), elapsed, block),
    ))
}

fn read_file(
    f: &mut impl Read,
    buf: &mut [u8],
    seed: &[u8],
    total: usize,
    what: &str,
) -> std::io::Result<usize> {
    let mut read_total = 0usize;
    loop {
        let n = f.read(buf)?;
        if n == 0 {
            break;
        }
        if buf[..n] != seed[..n] {
            return Err(std::io::Error::other(format!("{what}: mismatch")));
        }
        read_total += n;
    }
    if read_total < total {
        return Err(std::io::Error::other(format!(
            "{what}: short read {read_total}, want >= {total}"
        )));
    }
    Ok(read_total)
}

fn concurrency_gain(ctx: &TestCtx) -> Outcome {
    let threads = ctx.cfg.threads.max(1);
    let iters = ctx.cfg.iterations.max(1);
    let block = ctx.cfg.io_block_size.max(1);
    let ops = threads.saturating_mul(iters);
    let trials = ctx.cfg.perf_trials.max(1);
    let payload = vec![0xABu8; block];
    let mib_amount = (ops * block) as u64;

    let read_path = ctx.work("perf_gain_read.bin");
    check_ok!(fsutil::write_all_path(&read_path, &payload));
    check_ok!(fsutil::read_to_vec(&read_path));

    let mut rows = Vec::new();
    let path = &read_path;
    let body = &payload;

    check_out!(gain_row(
        &mut rows,
        trials,
        ops,
        iters,
        mib_amount,
        "read-reopen",
        "MiB/s",
        |n| {
            for _ in 0..n {
                let got = fsutil::read_to_vec(path)?;
                if got != *body {
                    return Err(std::io::Error::other("read mismatch"));
                }
            }
            Ok(())
        },
        |it| {
            time_threads(threads, it, |_, _| {
                let got = fsutil::read_to_vec(path)?;
                if got != *body {
                    return Err(std::io::Error::other("read mismatch"));
                }
                Ok(())
            })
        },
    ));

    check_out!(gain_row(
        &mut rows,
        trials,
        ops,
        iters,
        mib_amount,
        "read-held-fd",
        "MiB/s",
        |n| {
            let mut f = fsutil::open_ro(path)?;
            let mut buf = vec![0u8; body.len()];
            for _ in 0..n {
                f.seek(SeekFrom::Start(0))?;
                f.read_exact(&mut buf)?;
                if buf != *body {
                    return Err(std::io::Error::other("held-fd read mismatch"));
                }
            }
            Ok(())
        },
        |it| {
            let errors = AtomicUsize::new(0);
            let elapsed = time_workers(threads, |_, start_b, end_b| {
                let mut f = match fsutil::open_ro(path) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        eprintln!("    held-fd open: {e}");
                        errors.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                };
                start_b.wait();
                if let Some(f) = f.as_mut() {
                    let mut buf = vec![0u8; body.len()];
                    for i in 0..it {
                        if f.seek(SeekFrom::Start(0)).is_err()
                            || f.read_exact(&mut buf).is_err()
                            || buf != *body
                        {
                            eprintln!("    held-fd read {i}");
                            errors.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                end_b.wait();
            });
            ParallelResult {
                elapsed,
                errors: errors.load(Ordering::Relaxed),
            }
        },
    ));

    let write_paths: Vec<PathBuf> = (0..threads)
        .map(|t| ctx.work(&format!("perf_gain_w_{t}.bin")))
        .collect();
    for p in &write_paths {
        check_ok!(fsutil::write_all_path(p, &payload));
    }
    check_out!(gain_row(
        &mut rows,
        trials,
        ops,
        iters,
        mib_amount,
        "write-distinct",
        "MiB/s",
        |n| {
            for i in 0..n {
                fsutil::write_all_path(&write_paths[i % threads], body)?;
            }
            Ok(())
        },
        |it| time_threads(threads, it, |t, _| fsutil::write_all_path(
            &write_paths[t],
            body
        )),
    ));
    for p in &write_paths {
        check_bytes!(
            check_ok!(fsutil::read_to_vec(p)),
            payload,
            "write-distinct contents"
        );
    }

    check_out!(gain_row(
        &mut rows,
        trials,
        ops,
        iters,
        ops as u64,
        "open+close",
        "ops/s",
        |n| {
            for _ in 0..n {
                drop(fsutil::open_ro(path)?);
            }
            Ok(())
        },
        |it| {
            time_threads(threads, it, |_, _| {
                drop(fsutil::open_ro(path)?);
                Ok(())
            })
        },
    ));

    let mix_r = ctx.work("perf_gain_mix_r.bin");
    let mix_w: Vec<PathBuf> = (0..threads)
        .map(|t| ctx.work(&format!("perf_gain_mix_w_{t}.bin")))
        .collect();
    check_ok!(fsutil::write_all_path(&mix_r, &payload));
    for p in &mix_w {
        check_ok!(fsutil::write_all_path(p, &payload));
    }
    check_out!(gain_row(
        &mut rows,
        trials,
        ops,
        iters,
        (ops * 2) as u64,
        "mixed-rw",
        "ops/s",
        |n| {
            for _ in 0..n {
                let got = fsutil::read_to_vec(&mix_r)?;
                if got != *body {
                    return Err(std::io::Error::other("mix read mismatch"));
                }
            }
            for i in 0..n {
                fsutil::write_all_path(&mix_w[i % threads], body)?;
            }
            Ok(())
        },
        |it| {
            let errors = AtomicUsize::new(0);
            let n_ops = threads.saturating_mul(it);
            let elapsed = time_workers(threads + 1, |id, start_b, end_b| {
                start_b.wait();
                if id == 0 {
                    for _ in 0..n_ops {
                        match fsutil::read_to_vec(&mix_r) {
                            Ok(d) if d == *body => {}
                            _ => {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                } else {
                    let wpath = &mix_w[id - 1];
                    for _ in 0..it {
                        if fsutil::write_all_path(wpath, body).is_err() {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                end_b.wait();
            });
            ParallelResult {
                elapsed,
                errors: errors.load(Ordering::Relaxed),
            }
        },
    ));

    Outcome::PassNote(format_gain_note(threads, iters, block, ops, trials, &rows))
}

fn gain_row(
    rows: &mut Vec<Row>,
    trials: usize,
    ops: usize,
    iters: usize,
    amount: u64,
    name: &'static str,
    unit: &'static str,
    serial: impl FnMut(usize) -> std::io::Result<()>,
    parallel: impl FnMut(usize) -> ParallelResult,
) -> Result<(), Outcome> {
    let samples = gain_measure(trials, ops, iters, serial, parallel, name)?;
    rows.push(pick_row(name, unit, amount, &samples));
    Ok(())
}

fn format_gain_note(
    threads: usize,
    iters: usize,
    block: usize,
    ops: usize,
    trials: usize,
    rows: &[Row],
) -> String {
    let mut note = format!(
        "concurrency gain  ({} threads x {} iters, {} KiB ops, {} total ops)",
        threads,
        iters,
        block / 1024,
        ops
    );
    note.push('\n');
    if trials == 1 {
        note.push_str("  single trial; speedup = serial_time/parallel_time");
    } else {
        note.push_str(&format!(
            "  median-speedup trial of {trials} (rates paired); samples >= {}ms",
            MIN_SAMPLE.as_millis()
        ));
    }
    let scaled: Vec<_> = rows.iter().filter(|r| r.rounds > 1).collect();
    if !scaled.is_empty() {
        note.push('\n');
        note.push_str("  scaled:");
        for r in scaled {
            note.push_str(&format!(" {} x{}", r.name, r.rounds));
        }
    }
    note.push('\n');
    let minmax_hdr = if trials > 1 {
        format!("  {:>11}", "min-max")
    } else {
        String::new()
    };
    note.push_str(&format!(
        "  {:<16} {:>12} {:>12} {:>8}{minmax_hdr}",
        "op", "serial", "parallel", "speedup"
    ));
    for r in rows {
        note.push('\n');
        let minmax = if trials > 1 {
            format!("  {:>5.2}-{:<5.2}", r.speedup_min, r.speedup_max)
        } else {
            String::new()
        };
        note.push_str(&format!(
            "  {:<16} {:>8.1} {:<3} {:>8.1} {:<3} {:>7.2}x{minmax}",
            r.name, r.serial_rate, r.unit, r.parallel_rate, r.unit, r.speedup
        ));
    }
    note
}

struct Timed<T> {
    elapsed: Duration,
    result: T,
}

fn time(f: impl FnOnce() -> std::io::Result<()>) -> Timed<std::io::Result<()>> {
    let start = Instant::now();
    let result = f();
    Timed {
        elapsed: start.elapsed(),
        result,
    }
}

// Held-fd/open+close finish in ~100ms; a hitch then looks like a 3x swing.
const MIN_SAMPLE: Duration = Duration::from_millis(800);
const MAX_ROUNDS: usize = 32;

struct Sample {
    serial: Duration,
    parallel: Duration,
}

struct GainRuns {
    samples: Vec<Sample>,
    rounds: usize,
}

struct Row {
    name: &'static str,
    unit: &'static str,
    serial_rate: f64,
    parallel_rate: f64,
    speedup: f64,
    speedup_min: f64,
    speedup_max: f64,
    rounds: usize,
}

fn speedup_of(s: Duration, p: Duration) -> f64 {
    s.as_secs_f64() / p.as_secs_f64().max(1e-9)
}

fn rounds_for(serial: Duration, parallel: Duration) -> usize {
    let need = |d: Duration| {
        let t = d.as_secs_f64().max(1e-6);
        ((MIN_SAMPLE.as_secs_f64() / t).ceil() as usize).clamp(1, MAX_ROUNDS)
    };
    need(serial).max(need(parallel))
}

fn gain_measure(
    trials: usize,
    ops: usize,
    iters: usize,
    mut serial: impl FnMut(usize) -> std::io::Result<()>,
    mut parallel: impl FnMut(usize) -> ParallelResult,
    what: &str,
) -> Result<GainRuns, Outcome> {
    let trials = trials.max(1);
    let probe_s = time(|| serial(ops));
    if let Err(e) = probe_s.result {
        return Err(Outcome::Fail(format!("serial {what} probe: {e}")));
    }
    let probe_p = parallel(iters);
    if probe_p.errors != 0 {
        return Err(Outcome::Fail(format!(
            "parallel {what} probe: {} errors",
            probe_p.errors
        )));
    }
    let rounds = rounds_for(probe_s.elapsed, probe_p.elapsed);
    let mut samples = Vec::with_capacity(trials);
    let start = if rounds == 1 {
        samples.push(Sample {
            serial: probe_s.elapsed,
            parallel: probe_p.elapsed,
        });
        1
    } else {
        0
    };
    for trial in start..trials {
        let serial_t = time(|| serial(ops.saturating_mul(rounds)));
        if let Err(e) = serial_t.result {
            return Err(Outcome::Fail(format!("serial {what} #{trial}: {e}")));
        }
        let parallel_t = parallel(iters.saturating_mul(rounds));
        if parallel_t.errors != 0 {
            return Err(Outcome::Fail(format!(
                "parallel {what} #{trial}: {} errors",
                parallel_t.errors
            )));
        }
        samples.push(Sample {
            serial: serial_t.elapsed,
            parallel: parallel_t.elapsed,
        });
    }
    Ok(GainRuns { samples, rounds })
}

fn pick_row(name: &'static str, unit: &'static str, amount_one_round: u64, runs: &GainRuns) -> Row {
    let samples = &runs.samples;
    let amount = amount_one_round.saturating_mul(runs.rounds as u64);
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by(|&a, &b| {
        speedup_of(samples[a].serial, samples[a].parallel)
            .partial_cmp(&speedup_of(samples[b].serial, samples[b].parallel))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mid = &samples[order[order.len() / 2]];
    let mut sp_min = f64::INFINITY;
    let mut sp_max = 0.0f64;
    for s in samples {
        let sp = speedup_of(s.serial, s.parallel);
        sp_min = sp_min.min(sp);
        sp_max = sp_max.max(sp);
    }
    let s = mid.serial.as_secs_f64().max(1e-9);
    let p = mid.parallel.as_secs_f64().max(1e-9);
    let (serial_rate, parallel_rate) = if unit == "MiB/s" {
        let mib = amount as f64 / (1024.0 * 1024.0);
        (mib / s, mib / p)
    } else {
        (amount as f64 / s, amount as f64 / p)
    };
    Row {
        name,
        unit,
        serial_rate,
        parallel_rate,
        speedup: s / p,
        speedup_min: sp_min,
        speedup_max: sp_max,
        rounds: runs.rounds,
    }
}

fn throughput(label: &str, bytes: u64, elapsed: Duration, block: usize) -> String {
    let secs = elapsed.as_secs_f64().max(1e-9);
    let mib = bytes as f64 / (1024.0 * 1024.0);
    format!(
        "{label:5}  {mib:6.2} MiB in {secs:.3}s -> {:>7.1} MiB/s  ({} KiB)",
        mib / secs,
        block / 1024
    )
}
