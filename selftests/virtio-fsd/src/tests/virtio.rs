use std::io::Write;
use std::os::fd::AsRawFd;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_errno, check_errno_any, check_ok,
    fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![edge debug_nodes_stay_in_debug_scheme, edge forget_lru_pressure, edge mmap_file]
}

// The debug nodes are served by `virtio-fsd-debug.<tag>` only: the share must
// not expose them, and the debug scheme must not expose anything else.
fn debug_nodes_stay_in_debug_scheme(ctx: &TestCtx) -> Outcome {
    for name in ["lru_evicted", ".lru_evicted"] {
        check_errno!(
            fsutil::open_ro(&ctx.cfg.root.join(name)),
            fsutil::ENOENT,
            "share must not expose debug node"
        );
    }
    check_ok!(fsutil::open_ro(&ctx.cfg.debug_root.join("lru_evicted")));
    check_errno!(
        fsutil::open_ro(&ctx.cfg.debug_root.join("fixtures")),
        fsutil::ENOENT,
        "debug scheme must not expose share paths"
    );
    Outcome::Pass
}

// virtio-fsd INODES_MAX_LEN is 64k; this has to be past that to force FORGET.
const FORGET_PRESSURE_FILES: usize = 64 * 1024 + 256;

fn forget_lru_pressure(ctx: &TestCtx) -> Outcome {
    // The log lives in the driver's debug scheme, not in the share.
    check_ok!(
        fsutil::clear_lru_eviction_log(&ctx.cfg.debug_root),
        "truncate {}/lru_evicted",
        ctx.cfg.debug_root.display()
    );

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

    let log = check_ok!(fsutil::read_to_vec(&ctx.cfg.debug_root.join("lru_evicted")));
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
        "expected {run_token}* evictions in lru_evicted, got:\n{log}"
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
