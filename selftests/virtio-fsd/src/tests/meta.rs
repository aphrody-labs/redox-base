use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok,
    check_out, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func meta_fstat_size,
        func meta_fstat_after_write,
        func meta_fsync_file,
        func meta_fsync_dir,
        func meta_fstat_directory,
        func meta_ftruncate,
        func meta_o_trunc,
        func meta_futimens,
        func meta_futimens_now,
        func meta_futimens_omit_mtime,
        func meta_futimens_dir,
        func meta_fchmod,
        func meta_chmod_path,
        func meta_fchmod_dir,
        func meta_cp,
        func meta_cp_overwrite,
        func meta_cp_p,
        func meta_cp_from_host,
        func meta_cp_p_from_host,
        func meta_fchown_self,
        func meta_chown_path,
        func meta_fchown_dir,
        func meta_create_owner,
        func meta_create_mode,
        func meta_setattr_together,
        func meta_fstatvfs,
        edge edge_setattr_handles,
        edge edge_fstatvfs,
        edge edge_attr_cache_expiration,
    ]
}

fn vfs_sane(st: &libc::statvfs) -> bool {
    st.f_bsize > 0
        && st.f_frsize > 0
        && st.f_blocks > 0
        && st.f_bfree <= st.f_blocks
        && st.f_bavail <= st.f_bfree
}

fn vfs_core(st: &libc::statvfs) -> (u64, u64, u64, u64) {
    (
        st.f_bsize as u64,
        st.f_blocks as u64,
        st.f_bfree as u64,
        st.f_bavail as u64,
    )
}

// f_bfree moves between calls; compare the superblock fields only.
fn vfs_super(st: &libc::statvfs) -> (u64, u64, u64) {
    (st.f_bsize as u64, st.f_frsize as u64, st.f_blocks as u64)
}

fn unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn between(got: SystemTime, lo: SystemTime, hi: SystemTime) -> bool {
    got.duration_since(lo)
        .map(|d| d > Duration::from_secs(1))
        .unwrap_or(false)
        && hi
            .duration_since(got)
            .map(|d| d > Duration::from_secs(1))
            .unwrap_or(false)
}

fn uid_gid() -> (u32, u32) {
    (unsafe { libc::getuid() }, unsafe { libc::getgid() })
}

fn skip_if_cp_unusable(err: &std::io::Error) -> Option<Outcome> {
    if err.kind() == std::io::ErrorKind::NotFound {
        return Some(Outcome::Skip("no cp on PATH".into()));
    }
    let s = err.to_string();
    if s.contains("xattr") || s.contains("unsupported platform") {
        return Some(Outcome::Skip(
            "cp -p xattrs: uutils/xattr has no Redox support".into(),
        ));
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CpMode {
    Plain,
    Preserve,
}

fn cp_or_skip(src: &Path, dst: &Path, mode: CpMode, what: &str) -> Result<(), Outcome> {
    match fsutil::cp(src, dst, mode == CpMode::Preserve) {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(skip) = skip_if_cp_unusable(&err) {
                Err(skip)
            } else {
                Err(Outcome::Fail(format!("{what}: {err}")))
            }
        }
    }
}

struct CpExpect<'a> {
    content: &'a [u8],
    dst_mode: u16,
    src_mode: Option<u16>,
}

fn check_copy(src: &Path, dst: &Path, mode: CpMode, expect: CpExpect<'_>) -> Outcome {
    check_bytes!(check_ok!(fsutil::read_to_vec(dst)), expect.content);
    check_eq!(check_ok!(fsutil::mode_of(dst)), expect.dst_mode);
    if let Some(src_mode) = expect.src_mode {
        check_eq!(check_ok!(fsutil::mode_of(src)), src_mode);
        check_bytes!(check_ok!(fsutil::read_to_vec(src)), expect.content);
    }
    if mode == CpMode::Preserve {
        let src_st = check_ok!(fsutil::stat(src));
        let dst_st = check_ok!(fsutil::stat(dst));
        check_eq!(fsutil::owner(&dst_st), fsutil::owner(&src_st));
        let src_mtime = check_ok!(fsutil::mtime(src));
        let dst_mtime = check_ok!(fsutil::mtime(dst));
        check!(
            fsutil::close_enough(dst_mtime, src_mtime, 1),
            "preserve mtime {dst_mtime:?} != src {src_mtime:?}"
        );
    }
    Outcome::Pass
}

fn meta_fstat_size(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let len = check_ok!(fsutil::stat(&ctx.fixtures().join("files/small.txt"))).st_size as u64;
    check!(len > 0, "small.txt empty");
    Outcome::Pass
}

fn meta_fstat_after_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_size.txt");
    let payload = b"1234567890";
    check_ok!(fsutil::write_all_path(&path, payload));
    check_eq!(
        check_ok!(fsutil::stat(&path)).st_size as u64,
        payload.len() as u64
    );
    Outcome::Pass
}

fn meta_fsync_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_fsync.txt");
    let mut f = check_ok!(fsutil::create_rw(&path));
    check_ok!(f.write_all(b"sync-me"));
    check_ok!(fsutil::fsync(&f));
    Outcome::Pass
}

fn meta_fsync_dir(ctx: &TestCtx) -> Outcome {
    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    check_ok!(fsutil::fsync(&dir));
    Outcome::Pass
}

fn meta_fstat_directory(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let path = ctx.fixtures().join("nested");
    check_ok!(fsutil::open_dir(&path));
    let meta = check_ok!(std::fs::metadata(&path));
    check!(meta.is_dir(), "nested should be a directory");
    Outcome::Pass
}

fn meta_ftruncate(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_trunc.bin");
    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(4));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 4);
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"0123");

    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(8));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 8);
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"0123\0\0\0\0");

    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(8));
    drop(f);

    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(3));
    check_ok!(f.seek(SeekFrom::Start(0)));
    check_ok!(f.write_all(b"XYZ"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"XYZ");

    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(0));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 0);
    drop(f);
    check_eq!(check_ok!(fsutil::read_to_vec(&path)).len(), 0);

    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::unlink(&path));
    check_ok!(f.set_len(4));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 4);
    Outcome::Pass
}

fn meta_o_trunc(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_o_trunc.bin");
    check_ok!(fsutil::write_all_path(&path, b"keep-me-not"));
    let f = check_ok!(fsutil::open_flags(&path, flag::O_WRONLY | flag::O_TRUNC, 0));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 0);
    drop(f);
    check_eq!(check_ok!(fsutil::read_to_vec(&path)).len(), 0);

    check_ok!(fsutil::write_all_path(&path, b"old-payload"));
    let mut f = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_TRUNC, 0));
    check_ok!(f.write_all(b"new"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"new");

    check_ok!(fsutil::write_all_path(&path, b"stale"));
    let f = check_ok!(fsutil::create_rw(&path));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 0);
    drop(f);

    let dir = ctx.work("meta_o_trunc_dir");
    check_ok!(fsutil::make_dir(&dir));
    let child = dir.join("keep.txt");
    check_ok!(fsutil::write_all_path(&child, b"stay"));
    drop(check_ok!(fsutil::open_flags(
        &dir,
        flag::O_RDONLY | flag::O_DIRECTORY | flag::O_TRUNC,
        0
    )));
    check_eq!(check_ok!(fsutil::stat(&child)).st_size as u64, 4);
    Outcome::Pass
}

fn meta_futimens(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_futimens.bin");
    check_ok!(fsutil::write_all_path(&path, b"t"));
    let f = check_ok!(fsutil::open_rw(&path));
    let want = unix(1_700_000_000);
    check_ok!(fsutil::set_times_secs(&f, 1_700_000_000));
    drop(f);
    let got = check_ok!(fsutil::mtime(&path));
    check!(
        fsutil::close_enough(got, want, 1),
        "mtime {got:?} not within 1s of {want:?}"
    );
    Outcome::Pass
}

fn meta_futimens_now(ctx: &TestCtx) -> Outcome {
    // UTIME_NOW is the host virtiofsd clock, not guest SystemTime::now().
    let past = unix(1_600_000_000);
    let future = unix(2_000_000_000);
    let path = ctx.work("meta_futimens_now.bin");
    check_ok!(fsutil::write_all_path(&path, b"t"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::set_times_secs(&f, 1_600_000_000));
    check_ok!(fsutil::futimens_now(&f));
    let after_past = check_ok!(fsutil::mtime(&path));
    check!(
        !fsutil::close_enough(after_past, past, 1),
        "NOW left the planted past {after_past:?}"
    );

    check_ok!(fsutil::set_times_secs(&f, 2_000_000_000));
    check_ok!(fsutil::futimens_now(&f));
    drop(f);
    let after_future = check_ok!(fsutil::mtime(&path));
    check!(
        !fsutil::close_enough(after_future, future, 1),
        "NOW left the planted future {after_future:?}"
    );
    check!(
        fsutil::close_enough(after_past, after_future, 5),
        "two host NOWs disagreed: {after_past:?} vs {after_future:?}"
    );
    check!(
        between(after_past, past, future),
        "NOW {after_past:?} not between past {past:?} and future {future:?}"
    );
    Outcome::Pass
}

fn meta_futimens_omit_mtime(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_futimens_omit.bin");
    check_ok!(fsutil::write_all_path(&path, b"t"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::futimens_now(&f));
    let host_now = check_ok!(fsutil::mtime(&path));

    let planted_a = unix(1_650_000_000);
    let planted_m = unix(1_660_000_000);
    check_ok!(fsutil::futimens(
        &f,
        fsutil::timespec(1_650_000_000, 0),
        fsutil::timespec(1_660_000_000, 0),
    ));
    check_ok!(fsutil::futimens(
        &f,
        fsutil::timespec(0, fsutil::UTIME_NOW),
        fsutil::timespec(0, fsutil::UTIME_OMIT),
    ));
    drop(f);
    let mtime = check_ok!(fsutil::mtime(&path));
    check!(
        fsutil::close_enough(mtime, planted_m, 1),
        "mtime should stay planted, got {mtime:?}"
    );
    let atime = check_ok!(fsutil::atime(&path));
    check!(
        !fsutil::close_enough(atime, planted_a, 1),
        "atime still planted {atime:?}"
    );
    check!(
        !fsutil::close_enough(atime, planted_m, 1),
        "atime copied mtime {atime:?}"
    );
    check!(
        fsutil::close_enough(atime, host_now, 5),
        "atime NOW {atime:?} != host NOW {host_now:?}"
    );
    Outcome::Pass
}

fn meta_futimens_dir(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_futimens_dir");
    check_ok!(fsutil::make_dir(&path));
    let dir = check_ok!(fsutil::open_dir(&path));
    let want = unix(1_680_000_000);
    check_ok!(fsutil::set_times_secs(&dir, 1_680_000_000));
    drop(dir);
    let got = check_ok!(fsutil::mtime(&path));
    check!(
        fsutil::close_enough(got, want, 1),
        "dir mtime {got:?} not within 1s of {want:?}"
    );
    Outcome::Pass
}

fn meta_fchmod(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_fchmod.bin");
    let payload = b"cp-payload";
    check_ok!(fsutil::write_all_path(&path, payload));
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::fchmod(&f, 0o640));
    check_eq!(
        check_ok!(fsutil::fstat(&f)).st_size as u64,
        payload.len() as u64
    );
    drop(f);
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o640);
    check_eq!(check_ok!(fsutil::type_of(&path)), fsutil::MODE_FILE);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), payload);
    Outcome::Pass
}

fn meta_chmod_path(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_chmod.bin");
    let payload = b"chmod-path";
    check_ok!(fsutil::write_all_path(&path, payload));
    check_ok!(fsutil::chmod(&path, 0o600));
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o600);
    check_eq!(check_ok!(fsutil::type_of(&path)), fsutil::MODE_FILE);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), payload);
    Outcome::Pass
}

fn meta_fchmod_dir(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_fchmod_dir");
    check_ok!(fsutil::make_dir(&path));
    let dir = check_ok!(fsutil::open_dir(&path));
    check_ok!(fsutil::fchmod(&dir, 0o700));
    drop(dir);
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o700);
    check!(
        check_ok!(fsutil::type_of(&path)) != fsutil::MODE_FILE,
        "directory type bit lost"
    );
    Outcome::Pass
}

fn meta_cp(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("meta_cp_src.bin");
    let dst = ctx.work("meta_cp_dst.bin");
    let payload = b"copy-me-please";
    check_ok!(fsutil::write_all_path(&src, payload));
    check_ok!(fsutil::chmod(&src, 0o640));
    check_out!(cp_or_skip(&src, &dst, CpMode::Plain, "cp"));
    check_eq!(check_ok!(fsutil::type_of(&dst)), fsutil::MODE_FILE);
    check_copy(
        &src,
        &dst,
        CpMode::Plain,
        CpExpect {
            content: payload,
            dst_mode: 0o640,
            src_mode: Some(0o640),
        },
    )
}

fn meta_cp_overwrite(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("meta_cp_ow_src.bin");
    let dst = ctx.work("meta_cp_ow_dst.bin");
    check_ok!(fsutil::write_all_path(&src, b"new-bytes"));
    check_ok!(fsutil::chmod(&src, 0o600));
    check_ok!(fsutil::write_all_path(&dst, b"OLD-STALE-CONTENT"));
    check_ok!(fsutil::chmod(&dst, 0o666));
    check_out!(cp_or_skip(&src, &dst, CpMode::Plain, "cp overwrite"));
    // Plain cp onto an existing dest keeps the dest mode (uutils/GNU).
    check_copy(
        &src,
        &dst,
        CpMode::Plain,
        CpExpect {
            content: b"new-bytes",
            dst_mode: 0o666,
            src_mode: Some(0o600),
        },
    )
}

fn meta_cp_p(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("meta_cp_p_src.bin");
    let dst = ctx.work("meta_cp_p_dst.bin");
    check_ok!(fsutil::write_all_path(&src, b"preserve-me"));
    check_ok!(fsutil::chmod(&src, 0o640));
    check_out!(cp_or_skip(&src, &dst, CpMode::Preserve, "cp --preserve"));
    check_copy(
        &src,
        &dst,
        CpMode::Preserve,
        CpExpect {
            content: b"preserve-me",
            dst_mode: 0o640,
            src_mode: None,
        },
    )
}

fn meta_cp_from_host(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let src = ctx.fixtures().join("cp/src.txt");
    let src600 = ctx.fixtures().join("cp/src_600.txt");
    check_out!(fsutil::skip_if_missing(&src, "host cp/src.txt"));
    check_out!(fsutil::skip_if_missing(&src600, "host cp/src_600.txt"));
    let dst = ctx.work("meta_cp_host_dst.bin");
    check_out!(cp_or_skip(&src, &dst, CpMode::Plain, "cp from fixture"));
    check_eq!(check_ok!(fsutil::type_of(&dst)), fsutil::MODE_FILE);
    check_out!(
        check_copy(
            &src,
            &dst,
            CpMode::Plain,
            CpExpect {
                content: crate::config::CP_SRC_MARKER.as_bytes(),
                dst_mode: 0o640,
                src_mode: Some(0o640),
            },
        )
        .into_result()
    );

    let dst600 = ctx.work("meta_cp_host_600.bin");
    check_out!(cp_or_skip(
        &src600,
        &dst600,
        CpMode::Plain,
        "cp 0600 fixture"
    ));
    check_eq!(
        check_ok!(fsutil::read_to_vec(&dst600)),
        crate::config::CP_SRC_600_MARKER.as_bytes()
    );
    check_eq!(check_ok!(fsutil::mode_of(&dst600)), 0o600);
    Outcome::Pass
}

fn meta_cp_p_from_host(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let src = ctx.fixtures().join("cp/src.txt");
    check_out!(fsutil::skip_if_missing(&src, "host cp/src.txt"));
    let dst = ctx.work("meta_cp_p_host_dst.bin");
    check_out!(cp_or_skip(
        &src,
        &dst,
        CpMode::Preserve,
        "cp --preserve from fixture"
    ));
    check_copy(
        &src,
        &dst,
        CpMode::Preserve,
        CpExpect {
            content: crate::config::CP_SRC_MARKER.as_bytes(),
            dst_mode: 0o640,
            src_mode: None,
        },
    )
}

fn meta_fchown_self(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_fchown.bin");
    let payload = b"own";
    check_ok!(fsutil::write_all_path(&path, payload));
    check_ok!(fsutil::chmod(&path, 0o640));
    let (uid, gid) = uid_gid();
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::fchown(&f, uid, gid));
    check_eq!(fsutil::owner(&check_ok!(fsutil::fstat(&f))), (uid, gid));
    check_eq!(
        check_ok!(fsutil::fstat(&f)).st_size as u64,
        payload.len() as u64
    );
    drop(f);
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o640);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), payload);
    Outcome::Pass
}

fn meta_chown_path(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_chown.bin");
    check_ok!(fsutil::write_all_path(&path, b"own"));
    let (uid, gid) = uid_gid();
    check_ok!(fsutil::chown(&path, uid, gid));
    check_eq!(fsutil::owner(&check_ok!(fsutil::stat(&path))), (uid, gid));
    Outcome::Pass
}

fn meta_fchown_dir(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_fchown_dir");
    check_ok!(fsutil::make_dir(&path));
    let (uid, gid) = uid_gid();
    let dir = check_ok!(fsutil::open_dir(&path));
    check_ok!(fsutil::fchown(&dir, uid, gid));
    drop(dir);
    check_eq!(fsutil::owner(&check_ok!(fsutil::stat(&path))), (uid, gid));
    check_eq!(check_ok!(fsutil::type_of(&path)), fsutil::MODE_DIR);
    Outcome::Pass
}

fn meta_create_owner(ctx: &TestCtx) -> Outcome {
    // CREATE sends the guest uid; if the driver still stuffed 0 here we'd see it.
    let path = ctx.work("meta_create_owner.bin");
    check_ok!(fsutil::write_all_path(&path, b"ctx"));
    let (uid, gid) = uid_gid();
    check_eq!(fsutil::owner(&check_ok!(fsutil::stat(&path))), (uid, gid));
    Outcome::Pass
}

fn meta_create_mode(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("meta_create_600.bin");
    let b = ctx.work("meta_create_640.bin");
    drop(check_ok!(fsutil::open_flags(
        &a,
        flag::O_RDWR | flag::O_CREAT | flag::O_EXCL,
        0o600
    )));
    drop(check_ok!(fsutil::open_flags(
        &b,
        flag::O_RDWR | flag::O_CREAT | flag::O_EXCL,
        0o640
    )));
    check_eq!(check_ok!(fsutil::mode_of(&a)), 0o600);
    check_eq!(check_ok!(fsutil::mode_of(&b)), 0o640);
    check_eq!(check_ok!(fsutil::type_of(&a)), fsutil::MODE_FILE);
    Outcome::Pass
}

fn meta_setattr_together(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_setattr_together.bin");
    check_ok!(fsutil::write_all_path(&path, b"abcde"));
    let (uid, gid) = uid_gid();
    let want_m = unix(1_730_000_000);
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(3));
    check_ok!(fsutil::fchmod(&f, 0o640));
    check_ok!(fsutil::fchown(&f, uid, gid));
    check_ok!(fsutil::set_times_secs(&f, 1_730_000_000));
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 3);
    check_eq!(fsutil::owner(&check_ok!(fsutil::fstat(&f))), (uid, gid));
    drop(f);
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o640);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"abc");
    let mtime = check_ok!(fsutil::mtime(&path));
    check!(
        fsutil::close_enough(mtime, want_m, 1),
        "mtime {mtime:?} after mixed setattr"
    );
    Outcome::Pass
}

fn meta_fstatvfs(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("meta_statvfs.bin");
    check_ok!(fsutil::write_all_path(&path, b"statvfs"));
    let f = check_ok!(fsutil::open_ro(&path));
    let st = check_ok!(fsutil::fstatvfs(&f));
    check!(
        vfs_sane(&st),
        "implausible fstatvfs: bsize={} frsize={} blocks={} bfree={} bavail={}",
        st.f_bsize,
        st.f_frsize,
        st.f_blocks,
        st.f_bfree,
        st.f_bavail
    );
    check_eq!(
        st.f_frsize,
        st.f_bsize,
        "relibc copies f_bsize into f_frsize"
    );
    check_eq!(
        vfs_super(&st),
        vfs_super(&check_ok!(fsutil::statvfs(&path))),
        "fstatvfs vs statvfs"
    );

    let dir = ctx.work("meta_statvfs_dir");
    check_ok!(fsutil::make_dir(&dir));
    let df = check_ok!(fsutil::open_dir(&dir));
    let dst = check_ok!(fsutil::fstatvfs(&df));
    check!(vfs_sane(&dst), "dir fstatvfs");
    let file = dir.join("f.bin");
    check_ok!(fsutil::write_all_path(&file, b"x"));
    let ff = check_ok!(fsutil::open_ro(&file));
    check_eq!(
        vfs_super(&dst),
        vfs_super(&check_ok!(fsutil::fstatvfs(&ff))),
        "dir vs file"
    );

    for (what, file) in [
        ("O_STAT", check_ok!(fsutil::open_stat(&path))),
        ("O_WRONLY", check_ok!(fsutil::open_wo(&path))),
        ("unlinked", {
            let p = ctx.work("meta_statvfs_unlinked.bin");
            check_ok!(fsutil::write_all_path(&p, b"gone"));
            let f = check_ok!(fsutil::open_rw(&p));
            check_ok!(fsutil::unlink(&p));
            f
        }),
    ] {
        check!(
            vfs_sane(&check_ok!(fsutil::fstatvfs(&file), "{what}")),
            "{what} fstatvfs"
        );
    }

    let tgt = ctx.work("meta_statvfs_tgt.bin");
    let link = ctx.work("meta_statvfs_link");
    check_ok!(fsutil::write_all_path(&tgt, b"t"));
    check_ok!(fsutil::symlink("meta_statvfs_tgt.bin", &link));
    let lf = check_ok!(fsutil::open_symlink(&link));
    let lst = check_ok!(fsutil::fstatvfs(&lf));
    check!(vfs_sane(&lst), "symlink fstatvfs");
    check_eq!(
        vfs_core(&lst),
        vfs_core(&check_ok!(fsutil::statvfs(&link))),
        "link fd vs followed path"
    );

    let root = check_ok!(fsutil::open_dir(ctx.root()));
    let rst = check_ok!(fsutil::fstatvfs(&root));
    check!(vfs_sane(&rst), "root fstatvfs");
    check_eq!(
        vfs_super(&rst),
        vfs_super(&check_ok!(fsutil::statvfs(ctx.root()))),
        "root fd vs path"
    );
    Outcome::Pass
}

fn edge_setattr_handles(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_trunc.bin");
    check_ok!(fsutil::write_all_path(&path, b"data"));
    let dirp = ctx.work("edge_trunc_dir");
    check_ok!(fsutil::make_dir(&dirp));
    check_errno!(
        check_ok!(fsutil::open_dir(&dirp)).set_len(0),
        fsutil::EISDIR,
        "truncate dir"
    );
    check_errno!(
        check_ok!(fsutil::open_stat(&path)).set_len(1),
        fsutil::EBADF,
        "truncate O_STAT"
    );
    check_eq!(check_ok!(fsutil::stat(&path)).st_size as u64, 4);

    let tgt = ctx.work("edge_trunc_link_tgt.bin");
    let link = ctx.work("edge_trunc_link");
    check_ok!(fsutil::write_all_path(&tgt, b"0123456789"));
    check_ok!(fsutil::symlink("edge_trunc_link_tgt.bin", &link));
    check_errno!(
        check_ok!(fsutil::open_symlink(&link)).set_len(1),
        fsutil::EBADF,
        "truncate symlink"
    );
    check_eq!(check_ok!(fsutil::stat(&tgt)).st_size as u64, 10);
    check_errno!(
        check_ok!(fsutil::open_creat_symlink(&ctx.work("edge_trunc_presym"))).set_len(0),
        fsutil::EBADF,
        "truncate pending"
    );

    let a = check_ok!(fsutil::open_rw(&path));
    let b = check_ok!(fsutil::open_rw(&path));
    check_ok!(a.set_len(4));
    check_eq!(check_ok!(fsutil::fstat(&b)).st_size as u64, 4);

    let f = check_ok!(fsutil::open_stat(&path));
    check_ok!(fsutil::set_times_secs(&f, 1_710_000_000));
    drop(f);
    check!(
        fsutil::close_enough(check_ok!(fsutil::mtime(&path)), unix(1_710_000_000), 1),
        "mtime not applied via O_STAT"
    );

    let lf = check_ok!(fsutil::open_symlink(&link));
    check_ok!(fsutil::set_times_secs(&lf, 1_720_000_000));
    check_errno!(
        fsutil::futimens_now(&check_ok!(fsutil::open_creat_symlink(
            &ctx.work("edge_futimens_presym")
        ))),
        fsutil::EBADF,
        "futimens pending"
    );
    check_errno!(
        fsutil::futimens(
            &check_ok!(fsutil::open_rw(&path)),
            fsutil::timespec(0, 2_000_000_000),
            fsutil::timespec(0, 0),
        ),
        fsutil::EINVAL,
        "bad nsec"
    );

    check_errno!(
        fsutil::fchmod(
            &check_ok!(fsutil::open_creat_symlink(&ctx.work("edge_fchmod_presym"))),
            0o777
        ),
        fsutil::EBADF,
        "fchmod pending"
    );
    check_ok!(fsutil::chmod(&path, 0o777));
    let mode = check_ok!(fsutil::stat(&path)).st_mode as u16;
    check_eq!(mode & fsutil::MODE_TYPE, fsutil::MODE_FILE);
    check_eq!(mode & 0o777, 0o777);

    let (uid, gid) = uid_gid();
    check_errno!(
        fsutil::fchown(
            &check_ok!(fsutil::open_creat_symlink(&ctx.work("edge_fchown_presym"))),
            0,
            0
        ),
        fsutil::EBADF,
        "fchown pending"
    );
    let before = check_ok!(fsutil::stat(&path));
    let (ou, og) = fsutil::owner(&before);
    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::fchown(&f, u32::MAX, u32::MAX));
    check_ok!(fsutil::fchown(&f, u32::MAX, og));
    check_ok!(fsutil::fchown(&f, ou, u32::MAX));
    drop(f);
    let after = check_ok!(fsutil::stat(&path));
    let (nu, ng) = fsutil::owner(&after);
    check_eq!(nu, ou);
    check_eq!(ng, og);
    check!(nu != u32::MAX && ng != u32::MAX, "id became -1");

    let st = check_ok!(fsutil::open_stat(&path));
    check_ok!(fsutil::fchown(&st, uid, gid));
    check_ok!(fsutil::fchown(
        &check_ok!(fsutil::open_symlink(&link)),
        uid,
        gid
    ));
    check_eq!(fsutil::owner(&check_ok!(fsutil::lstat(&link))), (uid, gid));
    let a = check_ok!(fsutil::open_rw(&path));
    let b = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::fchown(&a, uid, gid));
    check_eq!(fsutil::owner(&check_ok!(fsutil::fstat(&b))), (uid, gid));
    check_ok!(fsutil::unlink(&path));
    check_ok!(fsutil::fchown(&a, uid, gid));
    Outcome::Pass
}

fn edge_fstatvfs(ctx: &TestCtx) -> Outcome {
    check_errno!(
        fsutil::fstatvfs(&check_ok!(fsutil::open_creat_symlink(
            &ctx.work("edge_statvfs_presym")
        ))),
        fsutil::EBADF,
        "pending symlink"
    );

    let path = ctx.work("edge_statvfs.bin");
    check_ok!(fsutil::write_all_path(&path, b"ab"));
    let f = check_ok!(fsutil::open_ro(&path));
    let fd = {
        use std::os::fd::AsRawFd;
        f.as_raw_fd()
    };
    drop(f);
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatvfs(fd, &mut st) };
    check!(rc < 0, "fstatvfs on closed fd succeeded");
    check_errno!(
        Err::<(), _>(std::io::Error::last_os_error()),
        fsutil::EBADF,
        "closed fd"
    );

    let a = check_ok!(fsutil::open_rw(&path));
    let b = check_ok!(fsutil::open_ro(&path));
    let sa = check_ok!(fsutil::fstatvfs(&a));
    let sb = check_ok!(fsutil::fstatvfs(&b));
    check_eq!(sa.f_bsize, sb.f_bsize);
    check_eq!(sa.f_blocks as u64, sb.f_blocks as u64);

    let mut g = check_ok!(fsutil::open_rw(&path));
    let before = check_ok!(fsutil::fstatvfs(&g));
    check_ok!(g.write_all(&[0u8; 4096]));
    check_ok!(fsutil::fsync(&g));
    let after = check_ok!(fsutil::fstatvfs(&g));
    check_eq!(after.f_bsize, before.f_bsize);
    check_eq!(after.f_blocks as u64, before.f_blocks as u64);

    let nest = ctx.work("edge_statvfs_nest");
    let inner = nest.join("inner");
    check_ok!(fsutil::make_dir(&nest));
    check_ok!(fsutil::make_dir(&inner));
    let sa = check_ok!(fsutil::fstatvfs(&check_ok!(fsutil::open_dir(&nest))));
    let sb = check_ok!(fsutil::fstatvfs(&check_ok!(fsutil::open_dir(&inner))));
    check_eq!(sa.f_bsize, sb.f_bsize);
    check_eq!(sa.f_blocks as u64, sb.f_blocks as u64);
    Outcome::Pass
}

fn edge_attr_cache_expiration(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_attr_expiry.bin");
    check_ok!(fsutil::write_all_path(&path, b"initial-data"));
    check_eq!(check_ok!(fsutil::stat(&path)).st_size as u64, 12);

    let f = check_ok!(fsutil::open_rw(&path));
    check_ok!(f.set_len(4));
    drop(f);

    std::thread::sleep(Duration::from_millis(1100));

    check_eq!(
        check_ok!(fsutil::stat(&path)).st_size as u64,
        4,
        "metadata length after cache expiration"
    );

    check_ok!(fsutil::chmod(&path, 0o600));
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o600);

    Outcome::Pass
}
