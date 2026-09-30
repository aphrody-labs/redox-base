#[cfg(feature = "lru-debug")]
use std::io::Write;
use std::os::fd::AsRawFd;

use crate::{Outcome, TestCase, TestCtx, cases, check_errno_any, check_ok, fsutil};
#[cfg(feature = "lru-debug")]
use crate::{check, check_bytes};

pub fn cases() -> Vec<TestCase> {
    #[cfg(feature = "lru-debug")]
    {
        cases![edge forget_lru_pressure, edge mmap_file]
    }
    #[cfg(not(feature = "lru-debug"))]
    {
        cases![edge mmap_file]
    }
}

// virtio-fsd INODES_MAX_LEN is 64k; this has to be past that to force FORGET.
#[cfg(feature = "lru-debug")]
const FORGET_PRESSURE_FILES: usize = 64 * 1024 + 256;

#[cfg(feature = "lru-debug")]
fn forget_lru_pressure(ctx: &TestCtx) -> Outcome {
    match fsutil::clear_lru_eviction_log(&ctx.cfg.root) {
        Ok(()) => {}
        Err(err) if fsutil::is_errno(&err, fsutil::ENOENT) => {
            return Outcome::PassNote(
                "driver built without lru-debug; eviction order not checked".into(),
            );
        }
        Err(err) => return Outcome::Fail(format!("truncate .lru_evicted: {err:?}")),
    }

    for i in 0..FORGET_PRESSURE_FILES {
        let path = ctx.work(&format!("fg_{i}"));
        let mut f = check_ok!(fsutil::create_rw(&path), "create {i}");
        check_ok!(f.write_all(b"x"), "write {i}");
        drop(f);
    }
    check_bytes!(check_ok!(fsutil::read_to_vec(&ctx.work("fg_0"))), b"x");
    check_bytes!(
        check_ok!(fsutil::read_to_vec(
            &ctx.work(&format!("fg_{}", FORGET_PRESSURE_FILES - 1))
        )),
        b"x"
    );

    let log = check_ok!(fsutil::read_to_vec(&ctx.cfg.root.join(".lru_evicted")));
    let log = check_ok!(std::str::from_utf8(&log));
    let run_token = format!("{}.{}.fg_", crate::config::WORK_PREFIX, ctx.cfg.run_id);
    let ours: Vec<usize> = log
        .lines()
        .filter_map(|line| {
            line.rsplit('/')
                .next()?
                .strip_prefix(&run_token)?
                .parse()
                .ok()
        })
        .collect();
    check!(
        !ours.is_empty(),
        "expected {run_token}* evictions in .lru_evicted, got:\n{log}"
    );
    check!(
        ours.windows(2).all(|w| w[0] < w[1]),
        "LRU evicted this run's fg_* out of order: {ours:?}"
    );
    check!(
        ours[0] == 0,
        "first evicted for this run should be fg_0, got {ours:?}"
    );
    Outcome::Pass
}

fn mmap_file(ctx: &TestCtx) -> Outcome {
    // mmap_prep is EOPNOTSUPP until DAX exists. Anything else is a surprise.
    let path = ctx.work("edge_mmap.bin");
    let page = 4096usize;
    check_ok!(fsutil::write_all_path(&path, &vec![0u8; page]));
    let f = check_ok!(fsutil::open_rw(&path));
    let ptr = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        let err = std::io::Error::last_os_error();
        check_errno_any!(
            Err::<(), _>(err),
            [
                fsutil::EOPNOTSUPP,
                fsutil::EINVAL,
                fsutil::ENOSYS,
                fsutil::ENODEV
            ],
            "mmap failed"
        );
        return Outcome::Pass;
    }
    unsafe {
        libc::munmap(ptr, page);
    }
    Outcome::PassNote("mmap succeeded; WAL shm may work".into())
}
