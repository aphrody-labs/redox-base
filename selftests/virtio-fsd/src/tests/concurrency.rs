use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::{Outcome, TestCase, TestCtx, cases, check, check_bytes, check_ok, check_out, fsutil};

pub fn cases() -> Vec<TestCase> {
    cases![
        concurrency io_concurrent_overlapping_writes,
        concurrency io_concurrent_overlapping_reads,
        concurrency io_concurrent_append,
        concurrency io_concurrent_read_write,
        concurrency io_concurrent_same_fd_writes,
        concurrency dma_pool_concurrency,
    ]
}

// Must stay within the host's `max_write` (virtiofsd default 1 MiB): the driver
// sends one FUSE_WRITE per call, so a write larger than that would be split by
// the caller's retry loop and no longer be atomic against other writers.
const CHUNK: usize = 64 * 1024;

fn join_ok<A, B, E: std::fmt::Display>(
    a: thread::ScopedJoinHandle<'_, Result<A, E>>,
    b: thread::ScopedJoinHandle<'_, Result<B, E>>,
    what: &str,
) -> Result<(A, B), Outcome> {
    match (a.join(), b.join()) {
        (Ok(Ok(va)), Ok(Ok(vb))) => Ok((va, vb)),
        (Ok(Err(e)), _) | (_, Ok(Err(e))) => Err(Outcome::Fail(format!("{what}: {e}"))),
        (Err(_), _) | (_, Err(_)) => Err(Outcome::Fail("worker panicked".into())),
    }
}

fn check_uniform(got: &[u8], a: u8, b: u8, what: &str) -> Result<(), Outcome> {
    let all_a = got.iter().all(|&x| x == a);
    let all_b = got.iter().all(|&x| x == b);
    if all_a || all_b {
        Ok(())
    } else {
        Err(Outcome::Fail(format!(
            "{what}: mixed bytes a={} b={} other={}",
            got.iter().filter(|&&x| x == a).count(),
            got.iter().filter(|&&x| x == b).count(),
            got.iter().filter(|&&x| x != a && x != b).count()
        )))
    }
}

fn io_concurrent_overlapping_writes(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("conc_overlap.bin");
    check_ok!(fsutil::write_all_path(&path, &vec![0u8; CHUNK]));
    let aa = vec![0xAAu8; CHUNK];
    let bb = vec![0xBBu8; CHUNK];

    let path_a = path.clone();
    let path_b = path.clone();
    let join = thread::scope(|scope| {
        let ha = scope.spawn(|| {
            let mut f = fsutil::open_rw(&path_a)?;
            f.write_all(&aa)
        });
        let hb = scope.spawn(|| {
            let mut f = fsutil::open_rw(&path_b)?;
            f.write_all(&bb)
        });
        join_ok(ha, hb, "concurrent write")
    });
    check_out!(join);

    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(got.len() == CHUNK, "len {} want {CHUNK}", got.len());
    check_out!(check_uniform(&got, 0xAA, 0xBB, "torn write"));
    Outcome::Pass
}

fn io_concurrent_overlapping_reads(ctx: &TestCtx) -> Outcome {
    // Two preads of overlapping but not identical ranges of a unique pattern.
    // Identical ranges would hide a crossed reply: both readers want the same bytes.
    let path = ctx.work("conc_overlap_r.bin");
    let overlap = CHUNK / 2;
    let len = CHUNK + overlap;
    let want: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    check_ok!(fsutil::write_all_path(&path, &want));

    let path_a = path.clone();
    let path_b = path.clone();
    let join = thread::scope(|scope| {
        let ha = scope.spawn(|| {
            let f = fsutil::open_ro(&path_a)?;
            fsutil::pread(&f, 0, CHUNK)
        });
        let hb = scope.spawn(|| {
            let f = fsutil::open_ro(&path_b)?;
            fsutil::pread(&f, overlap as u64, CHUNK)
        });
        join_ok(ha, hb, "concurrent read")
    });
    let (a, b) = check_out!(join);
    check_bytes!(a, &want[..CHUNK], "reader a [0, CHUNK)");
    check_bytes!(
        b,
        &want[overlap..overlap + CHUNK],
        "reader b [overlap, end)"
    );
    Outcome::Pass
}

fn io_concurrent_append(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("conc_append.bin");
    check_ok!(fsutil::write_all_path(&path, b""));
    const N: usize = 32;
    const BLOCK: usize = 4096;

    let join = thread::scope(|scope| {
        let path_a = path.clone();
        let path_b = path.clone();
        let ha = scope.spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&path_a)?;
            for _ in 0..N {
                f.write_all(&[0xAAu8; BLOCK])?;
            }
            Ok::<_, std::io::Error>(())
        });
        let hb = scope.spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&path_b)?;
            for _ in 0..N {
                f.write_all(&[0xBBu8; BLOCK])?;
            }
            Ok::<_, std::io::Error>(())
        });
        join_ok(ha, hb, "concurrent append")
    });
    check_out!(join);

    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(
        got.len() == 2 * N * BLOCK,
        "append size {} want {}",
        got.len(),
        2 * N * BLOCK
    );
    for chunk in got.chunks_exact(BLOCK) {
        check_out!(check_uniform(chunk, 0xAA, 0xBB, "torn append block"));
    }
    Outcome::Pass
}

fn io_concurrent_read_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("conc_rw.bin");
    check_ok!(fsutil::write_all_path(&path, &vec![0u8; CHUNK]));
    let cc = vec![0xCCu8; CHUNK];

    let path_w = path.clone();
    let path_r = path.clone();
    let join = thread::scope(|scope| {
        let hw = scope.spawn(|| {
            let mut f = fsutil::open_rw(&path_w)?;
            f.write_all(&cc)
        });
        let hr = scope.spawn(|| {
            let mut f = fsutil::open_ro(&path_r)?;
            let mut buf = vec![0u8; CHUNK];
            f.read_exact(&mut buf)?;
            Ok::<_, std::io::Error>(buf)
        });
        join_ok(hw, hr, "concurrent read/write")
    });
    let (_w, snapshot) = check_out!(join);
    check!(
        snapshot.len() == CHUNK,
        "read len {} want {CHUNK}",
        snapshot.len()
    );
    check_out!(check_uniform(&snapshot, 0, 0xCC, "torn read"));

    let got = check_ok!(fsutil::read_to_vec(&path));
    check_bytes!(got, cc);
    Outcome::Pass
}

fn io_concurrent_same_fd_writes(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("conc_same_fd.bin");
    check_ok!(fsutil::write_all_path(&path, &vec![0u8; CHUNK]));
    let aa = vec![0xAAu8; CHUNK];
    let bb = vec![0xBBu8; CHUNK];
    let f = check_ok!(fsutil::open_rw(&path));
    let fa = check_ok!(f.try_clone());
    let fb = check_ok!(f.try_clone());

    let join = thread::scope(|scope| {
        let ha = scope.spawn(|| {
            let mut f = fa;
            f.write_all(&aa)
        });
        let hb = scope.spawn(|| {
            let mut f = fb;
            f.write_all(&bb)
        });
        join_ok(ha, hb, "same-fd concurrent write")
    });
    check_out!(join);

    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(got.len() == CHUNK, "len {} want {CHUNK}", got.len());
    check_out!(check_uniform(&got, 0xAA, 0xBB, "torn same-fd write"));
    Outcome::Pass
}

fn dma_pool_concurrency(ctx: &TestCtx) -> Outcome {
    let threads = ctx.cfg.threads.clamp(1, 4);
    let iters = 16;
    let failed = Arc::new(AtomicBool::new(false));

    thread::scope(|scope| {
        for tid in 0..threads {
            let root = ctx.root();
            let run_id = &ctx.cfg.run_id;
            let failed = Arc::clone(&failed);

            scope.spawn(move || {
                for i in 0..iters {
                    let name = format!("{}.{}.dma_t{tid}_{i}", crate::config::WORK_PREFIX, run_id);
                    let path = root.join(&name);
                    match fsutil::create_rw(&path) {
                        Ok(mut f) => {
                            let _ = f.write_all(b"dma-payload");
                            drop(f);
                            let _ = fsutil::unlink(&path);
                        }
                        Err(e) => {
                            if !fsutil::is_errno(&e, fsutil::EAGAIN)
                                && !fsutil::is_errno(&e, fsutil::EWOULDBLOCK)
                                && !fsutil::is_errno(&e, libc::ENOMEM)
                            {
                                failed.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    check!(
        !failed.load(Ordering::SeqCst),
        "concurrent worker hit unexpected error outside EAGAIN/ENOMEM"
    );

    let probe = ctx.work("dma_probe.txt");
    check_ok!(fsutil::write_all_path(&probe, b"healthy"));
    check_bytes!(check_ok!(fsutil::read_to_vec(&probe)), b"healthy");
    check_ok!(fsutil::unlink(&probe));

    Outcome::Pass
}
