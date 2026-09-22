// Locks are guest-local (SchemeState), not FUSE.

use std::ffi::CString;
use std::io::Write;

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check_eq, check_errno, check_errno_any, check_ok, fsutil,
};

// SQLite's fcntl lock bytes (os.h). They sit past EOF on purpose.
const PENDING_BYTE: i64 = 0x4000_0000;
const RESERVED_BYTE: i64 = PENDING_BYTE + 1;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;

pub fn cases() -> Vec<TestCase> {
    cases![
        func lock_ofd,
        func lock_process,
        func lock_sqlite,
        func lock_two_pids,
        edge edge_lock_setlkw,
        edge edge_lock_pending_symlink,
    ]
}

fn flock_new(typ: i16, start: i64, len: i64) -> fsutil::Flock {
    fsutil::Flock {
        l_type: typ,
        l_whence: fsutil::SEEK_SET,
        l_start: start,
        l_len: len,
        l_pid: 0,
    }
}

fn setlk(file: &std::fs::File, cmd: i32, typ: i16, start: i64, len: i64) -> std::io::Result<i32> {
    let mut fl = flock_new(typ, start, len);
    fsutil::fcntl_lk(file, cmd, &mut fl)
}

fn ofd(file: &std::fs::File, typ: i16, start: i64, len: i64) -> std::io::Result<i32> {
    setlk(file, fsutil::F_OFD_SETLK, typ, start, len)
}

fn sqlite_shared(file: &std::fs::File, shared_off: i64) -> std::io::Result<()> {
    ofd(file, fsutil::F_RDLCK, PENDING_BYTE, 1)?;
    ofd(file, fsutil::F_RDLCK, SHARED_FIRST + shared_off, 1)?;
    ofd(file, fsutil::F_UNLCK, PENDING_BYTE, 1)?;
    Ok(())
}

fn pair(ctx: &TestCtx, name: &str) -> std::io::Result<(std::fs::File, std::fs::File)> {
    let path = ctx.work(name);
    fsutil::write_all_path(&path, b"lock")?;
    Ok((fsutil::open_rw(&path)?, fsutil::open_rw(&path)?))
}

fn blocked(res: std::io::Result<i32>, what: &str) -> Result<(), Outcome> {
    match res {
        Ok(_) => Err(Outcome::Fail(format!(
            "{what}: expected EWOULDBLOCK, succeeded"
        ))),
        Err(e) if fsutil::is_wouldblock(&e) => Ok(()),
        Err(e) => Err(Outcome::Fail(format!(
            "{what}: expected EWOULDBLOCK, got {e:?}"
        ))),
    }
}

macro_rules! check_blocked {
    ($expr:expr, $what:expr) => {
        if let Err(o) = blocked($expr, $what) {
            return o;
        }
    };
}

fn lock_ofd(ctx: &TestCtx) -> Outcome {
    let (a, b) = check_ok!(pair(ctx, "lock_ofd.bin"));
    let mut wr = flock_new(fsutil::F_WRLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&a, fsutil::F_OFD_SETLK, &mut wr));
    check_blocked!(
        fsutil::fcntl_lk(&b, fsutil::F_OFD_SETLK, &mut wr),
        "conflict b"
    );

    let mut un = flock_new(fsutil::F_UNLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&a, fsutil::F_OFD_SETLK, &mut un));
    check_ok!(fsutil::fcntl_lk(&b, fsutil::F_OFD_SETLK, &mut wr));
    check_ok!(fsutil::fcntl_lk(&b, fsutil::F_OFD_SETLK, &mut un));

    let mut rd = flock_new(fsutil::F_RDLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&a, fsutil::F_OFD_SETLK, &mut rd));
    check_ok!(fsutil::fcntl_lk(&b, fsutil::F_OFD_SETLK, &mut rd));
    check_blocked!(
        fsutil::fcntl_lk(&b, fsutil::F_OFD_SETLK, &mut wr),
        "shared blocks exclusive"
    );

    let mut q = flock_new(fsutil::F_RDLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&b, fsutil::F_OFD_GETLK, &mut q));
    drop(a);
    drop(b);
    let (a, b) = check_ok!(pair(ctx, "lock_ofd_get.bin"));
    check_ok!(fsutil::fcntl_lk(&a, fsutil::F_OFD_SETLK, &mut wr));
    let mut q = flock_new(fsutil::F_RDLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&b, fsutil::F_OFD_GETLK, &mut q));
    check_eq!(q.l_type, fsutil::F_WRLCK);
    check_eq!(q.l_pid, -1);
    let mut q = flock_new(fsutil::F_WRLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&a, fsutil::F_OFD_GETLK, &mut q));
    check_eq!(q.l_type, fsutil::F_UNLCK);

    let (a, b) = check_ok!(pair(ctx, "lock_ofd_range.bin"));
    check_ok!(ofd(&a, fsutil::F_WRLCK, 0, 10));
    check_blocked!(ofd(&b, fsutil::F_WRLCK, 5, 10), "overlap");
    check_ok!(ofd(&b, fsutil::F_WRLCK, 10, 10));

    let (a, b) = check_ok!(pair(ctx, "lock_ofd_close.bin"));
    check_ok!(ofd(&a, fsutil::F_WRLCK, 0, 0));
    drop(a);
    check_ok!(ofd(&b, fsutil::F_WRLCK, 0, 0));

    let (mut a, b) = check_ok!(pair(ctx, "lock_ofd_io.bin"));
    check_ok!(ofd(&a, fsutil::F_WRLCK, 0, 0));
    check_ok!(a.write_all(b"xxxx"));
    check_blocked!(ofd(&b, fsutil::F_WRLCK, 0, 0), "after I/O");

    let (a, b) = check_ok!(pair(ctx, "lock_ofd_down.bin"));
    check_ok!(ofd(&a, fsutil::F_WRLCK, 0, 0));
    check_ok!(ofd(&a, fsutil::F_RDLCK, 0, 0));
    check_blocked!(ofd(&b, fsutil::F_WRLCK, 0, 0), "exclusive vs downgraded");

    let dirp = ctx.work("lock_ofd_dir");
    let d = check_ok!(fsutil::mkdir(&dirp));
    check_ok!(ofd(&d, fsutil::F_WRLCK, 0, 0));

    let a_path = ctx.work("lock_ofd_a.bin");
    let b_path = ctx.work("lock_ofd_b.bin");
    check_ok!(fsutil::write_all_path(&a_path, b"a"));
    check_ok!(fsutil::write_all_path(&b_path, b"b"));
    let fa = check_ok!(fsutil::open_rw(&a_path));
    let fb = check_ok!(fsutil::open_rw(&b_path));
    check_ok!(ofd(&fa, fsutil::F_WRLCK, 0, 0));
    check_ok!(ofd(&fb, fsutil::F_WRLCK, 0, 0));
    Outcome::Pass
}

fn lock_process(ctx: &TestCtx) -> Outcome {
    let (a, b) = check_ok!(pair(ctx, "lock_setlk.bin"));
    check_ok!(setlk(&a, fsutil::F_SETLK, fsutil::F_WRLCK, 0, 0));
    check_ok!(setlk(&b, fsutil::F_SETLK, fsutil::F_WRLCK, 0, 0));

    let path = ctx.work("lock_getlk.bin");
    check_ok!(fsutil::write_all_path(&path, b"lock"));
    let f = check_ok!(fsutil::open_rw(&path));
    let mut fl = flock_new(fsutil::F_WRLCK, 0, 0);
    check_ok!(fsutil::fcntl_lk(&f, fsutil::F_GETLK, &mut fl));
    check_eq!(fl.l_type, fsutil::F_UNLCK);

    let (a, b) = check_ok!(pair(ctx, "lock_setlk_ofd.bin"));
    check_ok!(setlk(&a, fsutil::F_SETLK, fsutil::F_WRLCK, 0, 0));
    check_blocked!(ofd(&b, fsutil::F_WRLCK, 0, 0), "OFD vs process lock");

    check_ok!(setlk(&f, fsutil::F_SETLK, fsutil::F_WRLCK, PENDING_BYTE, 1));
    Outcome::Pass
}

fn lock_sqlite(ctx: &TestCtx) -> Outcome {
    let (a, b) = check_ok!(pair(ctx, "lock_sqlite.bin"));
    check_ok!(sqlite_shared(&a, 0));
    check_ok!(sqlite_shared(&b, 1));
    check_ok!(ofd(&b, fsutil::F_WRLCK, RESERVED_BYTE, 1));

    let (a, b) = check_ok!(pair(ctx, "lock_sqlite_ex.bin"));
    check_ok!(sqlite_shared(&a, 3));
    check_ok!(ofd(&b, fsutil::F_WRLCK, PENDING_BYTE, 1));
    check_blocked!(
        ofd(&b, fsutil::F_WRLCK, SHARED_FIRST, SHARED_SIZE),
        "EXCLUSIVE vs SHARED"
    );

    let (a, b) = check_ok!(pair(ctx, "lock_sqlite_pend.bin"));
    check_ok!(ofd(&b, fsutil::F_WRLCK, PENDING_BYTE, 1));
    check_blocked!(ofd(&a, fsutil::F_RDLCK, PENDING_BYTE, 1), "new SHARED");

    let (a, b) = check_ok!(pair(ctx, "lock_setlk_pend.bin"));
    check_ok!(setlk(&a, fsutil::F_SETLK, fsutil::F_WRLCK, PENDING_BYTE, 1));
    check_blocked!(ofd(&b, fsutil::F_WRLCK, PENDING_BYTE, 1), "OFD PENDING");
    Outcome::Pass
}

fn lock_two_pids(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("lock_2pid.bin");
    check_ok!(fsutil::write_all_path(&path, b"db"));
    let cpath = check_ok!(CString::new(fsutil::path_to_str(&path).unwrap_or("")));

    let mut c2p = [0; 2];
    let mut p2c = [0; 2];
    if unsafe { libc::pipe(c2p.as_mut_ptr()) } != 0 || unsafe { libc::pipe(p2c.as_mut_ptr()) } != 0
    {
        return Outcome::Skip("pipe() failed".into());
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(c2p[0]);
            libc::close(c2p[1]);
            libc::close(p2c[0]);
            libc::close(p2c[1]);
        }
        return Outcome::Skip("fork() not available".into());
    }

    if pid == 0 {
        unsafe {
            libc::close(c2p[0]);
            libc::close(p2c[1]);
        }
        let fd = unsafe { libc::open(cpath.as_ptr(), flag::O_RDWR as i32) };
        if fd < 0 {
            unsafe { libc::_exit(2) };
        }
        let mut fl = flock_new(fsutil::F_WRLCK, PENDING_BYTE, 1);
        let rc = unsafe { libc::fcntl(fd, fsutil::F_SETLK, &mut fl) };
        if rc != 0 {
            unsafe { libc::_exit(2) };
        }
        let one = [1u8];
        unsafe {
            libc::write(c2p[1], one.as_ptr().cast(), 1);
            let mut b = [0u8; 1];
            libc::read(p2c[0], b.as_mut_ptr().cast(), 1);
            libc::close(fd);
            libc::_exit(0);
        }
    }

    unsafe {
        libc::close(c2p[1]);
        libc::close(p2c[0]);
    }
    let f = check_ok!(fsutil::open_rw(&path));
    let mut got = [0u8; 1];
    let n = unsafe { libc::read(c2p[0], got.as_mut_ptr().cast(), 1) };
    unsafe {
        libc::close(c2p[0]);
    }
    if n != 1 {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, core::ptr::null_mut(), 0);
            libc::close(p2c[1]);
        }
        return Outcome::Fail("child did not take F_SETLK".into());
    }

    let mut q = flock_new(fsutil::F_WRLCK, PENDING_BYTE, 1);
    let getlk = fsutil::fcntl_lk(&f, fsutil::F_GETLK, &mut q);
    let while_held = setlk(&f, fsutil::F_SETLK, fsutil::F_WRLCK, PENDING_BYTE, 1);

    let one = [1u8];
    unsafe {
        libc::write(p2c[1], one.as_ptr().cast(), 1);
        libc::close(p2c[1]);
        libc::waitpid(pid, core::ptr::null_mut(), 0);
    }

    check_ok!(getlk);
    check_eq!(q.l_type, fsutil::F_WRLCK);
    check_eq!(q.l_pid, pid as i32);
    check_blocked!(while_held, "parent SETLK while child holds");

    let mut last_err = None;
    for _ in 0..100 {
        match setlk(&f, fsutil::F_SETLK, fsutil::F_WRLCK, PENDING_BYTE, 1) {
            Ok(_) => return Outcome::Pass,
            Err(e) if fsutil::is_wouldblock(&e) => {
                last_err = Some(e);
                unsafe {
                    libc::sched_yield();
                }
            }
            Err(e) => return Outcome::Fail(format!("F_SETLK after child exit: {e:?}")),
        }
    }
    Outcome::Fail(format!(
        "F_SETLK after child exit still blocked: {:?}",
        last_err
    ))
}

fn edge_lock_setlkw(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("lock_setlkw.bin");
    check_ok!(fsutil::write_all_path(&path, b"lock"));
    let f = check_ok!(fsutil::open_rw(&path));
    let mut fl = flock_new(fsutil::F_WRLCK, 0, 0);
    check_errno!(
        fsutil::fcntl_lk(&f, fsutil::F_SETLKW, &mut fl),
        fsutil::EINVAL,
        "F_SETLKW"
    );
    check_errno!(
        fsutil::fcntl_lk(&f, fsutil::F_OFD_SETLKW, &mut fl),
        fsutil::EINVAL,
        "F_OFD_SETLKW"
    );
    Outcome::Pass
}

fn edge_lock_pending_symlink(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("lock_psym");
    let f = check_ok!(fsutil::open_creat_symlink(&path));
    check_errno_any!(
        ofd(&f, fsutil::F_WRLCK, 0, 0),
        [fsutil::EOPNOTSUPP, fsutil::EBADF],
        "lock pending"
    );
    Outcome::Pass
}
