use std::io::{Read, Seek, SeekFrom, Write};

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func fcntl_getfl_setfl,
        func io_append,
        func fcntl_accmode,
        edge edge_fcntl_handles,
    ]
}

fn fcntl_getfl_setfl(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fcntl_getfl.bin");
    check_ok!(fsutil::write_all_path(&path, b"x"));
    let f = check_ok!(fsutil::open_rw(&path));
    let fl = check_ok!(fsutil::fcntl_getfl(&f));
    check!(
        fl & libc::O_ACCMODE == libc::O_RDWR,
        "expected O_RDWR, got {fl:#x}"
    );
    check!(fl & libc::O_APPEND == 0, "O_APPEND set on a normal open");
    check_ok!(fsutil::fcntl_setfl(&f, fl | libc::O_APPEND));
    check!(
        check_ok!(fsutil::fcntl_getfl(&f)) & libc::O_APPEND != 0,
        "O_APPEND missing after SETFL"
    );
    check_ok!(fsutil::fcntl(&f, libc::F_GETFD, 0));
    check_ok!(fsutil::fcntl(&f, libc::F_SETFD, libc::FD_CLOEXEC));
    check_errno!(fsutil::fcntl(&f, 99, 0), fsutil::EINVAL, "unknown fcntl");

    let fl = check_ok!(fsutil::fcntl_getfl(&f));
    check_ok!(fsutil::fcntl_setfl(&f, fl | libc::O_NONBLOCK));
    check!(
        check_ok!(fsutil::fcntl_getfl(&f)) & libc::O_NONBLOCK != 0,
        "O_NONBLOCK missing after SETFL"
    );
    Outcome::Pass
}

fn io_append(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fcntl_append.bin");
    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut f = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_APPEND, 0));
    check!(
        check_ok!(fsutil::fcntl_getfl(&f)) & libc::O_APPEND != 0,
        "O_APPEND missing after open"
    );
    check_ok!(f.write_all(b"B"));
    check_ok!(f.write_all(b"C"));
    check_ok!(f.seek(SeekFrom::Start(0)));
    check_ok!(f.write_all(b"XY"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"AAAABCXY");

    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut f = check_ok!(fsutil::open_rw(&path));
    let fl = check_ok!(fsutil::fcntl_getfl(&f));
    check_ok!(f.seek(SeekFrom::Start(0)));
    check_ok!(fsutil::fcntl_setfl(&f, fl | libc::O_APPEND));
    check_ok!(f.write_all(b"B"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"AAAAB");

    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut f = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_APPEND, 0));
    let fl = check_ok!(fsutil::fcntl_getfl(&f));
    check_ok!(fsutil::fcntl_setfl(&f, fl & !libc::O_APPEND));
    check_ok!(f.seek(SeekFrom::Start(0)));
    check_ok!(f.write_all(b"B"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"BAAA");

    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut wo = check_ok!(fsutil::open_flags(
        &path,
        flag::O_WRONLY | flag::O_APPEND,
        0
    ));
    check_ok!(wo.write_all(b"Z"));
    drop(wo);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"AAAAZ");

    let mut trunc = check_ok!(fsutil::open_flags(
        &path,
        flag::O_RDWR | flag::O_APPEND | flag::O_TRUNC,
        0
    ));
    check_ok!(trunc.write_all(b"N"));
    drop(trunc);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"N");

    let creat = ctx.work("fcntl_append_creat.bin");
    let mut c = check_ok!(fsutil::open_flags(
        &creat,
        flag::O_RDWR | flag::O_CREAT | flag::O_APPEND | flag::O_EXCL,
        0o644,
    ));
    check_ok!(c.write_all(b"ab"));
    drop(c);
    check_bytes!(check_ok!(fsutil::read_to_vec(&creat)), b"ab");

    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    let at = ctx.work("fcntl_append_at.bin");
    check_ok!(fsutil::write_all_path(&at, b"AA"));
    let mut fat = check_ok!(fsutil::openat(
        &dir,
        fsutil::base_name(&at),
        flag::O_RDWR | flag::O_APPEND,
    ));
    check_ok!(fat.write_all(b"B"));
    drop(fat);
    check_bytes!(check_ok!(fsutil::read_to_vec(&at)), b"AAB");

    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut a = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_APPEND, 0));
    let mut b = check_ok!(fsutil::open_rw(&path));
    check_ok!(a.write_all(b"X"));
    check_ok!(b.seek(SeekFrom::Start(0)));
    check_ok!(b.write_all(b"Y"));
    drop(a);
    drop(b);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(
        got.starts_with(b"Y") || got.contains(&b'X'),
        "two-fd append {got:?}"
    );

    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let mut f = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_APPEND, 0));
    check_ok!(f.write_all(b"B"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 5);

    check_ok!(fsutil::write_all_path(&path, b""));
    let mut f = check_ok!(fsutil::open_flags(&path, flag::O_RDWR | flag::O_APPEND, 0));
    check_ok!(f.write_all(b""));
    check_ok!(f.write_all(b"Z"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"Z");
    Outcome::Pass
}

fn fcntl_accmode(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fcntl_acc.bin");
    check_ok!(fsutil::write_all_path(&path, b"hello"));
    let mut ro = check_ok!(fsutil::open_ro(&path));
    let mut buf = [0u8; 5];
    check_eq!(check_ok!(ro.read(&mut buf)), 5);
    check_eq!(&buf, b"hello");
    check_errno!(ro.write_all(b"x"), fsutil::EBADF, "write RO");
    // Zero-byte write on a regular file is a no-op; Redox returns Ok.
    match ro.write_all(b"") {
        Ok(()) => {}
        Err(e) if fsutil::is_errno(&e, fsutil::EBADF) => {}
        Err(e) => {
            return Outcome::Fail(format!("empty write RO: expected Ok or EBADF, got {e:?}"));
        }
    }

    let mut wo = check_ok!(fsutil::open_wo(&path));
    check_ok!(wo.write_all(b"BB"));
    check_errno!(wo.read(&mut buf), fsutil::EBADF, "read WO");
    check_errno!(wo.read(&mut []), fsutil::EBADF, "empty read WO");
    drop(wo);
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"BBllo");

    check_errno!(
        fsutil::open_flags(&path, flag::O_RDONLY | flag::O_APPEND, 0)
            .and_then(|mut f| f.write_all(b"x")),
        fsutil::EBADF,
        "O_RDONLY|O_APPEND write"
    );
    Outcome::Pass
}

fn edge_fcntl_handles(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fcntl_h.bin");
    check_ok!(fsutil::write_all_path(&path, b"data"));
    let dirp = ctx.work("fcntl_dir");
    check_ok!(fsutil::make_dir(&dirp));
    let tgt = ctx.work("fcntl_link_tgt.bin");
    let link = ctx.work("fcntl_link");
    check_ok!(fsutil::write_all_path(&tgt, b"t"));
    check_ok!(fsutil::symlink("fcntl_link_tgt.bin", &link));

    for (what, f) in [
        ("O_STAT", check_ok!(fsutil::open_stat(&path))),
        ("dir", check_ok!(fsutil::open_dir(&dirp))),
        ("symlink", check_ok!(fsutil::open_symlink(&link))),
        (
            "pending",
            check_ok!(fsutil::open_creat_symlink(&ctx.work("fcntl_presym"))),
        ),
    ] {
        check_ok!(fsutil::fcntl_getfl(&f), "F_GETFL {what}");
    }

    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::unlink(&path));
    let fl = check_ok!(fsutil::fcntl_getfl(&f));
    check_ok!(fsutil::fcntl_setfl(&f, fl | libc::O_APPEND));
    check_ok!(f.write_all(b"B"));

    let other = ctx.work("fcntl_other.bin");
    check_ok!(fsutil::write_all_path(&other, b"AAAA"));
    let mut a = check_ok!(fsutil::open_rw(&other));
    let mut b = check_ok!(fsutil::open_rw(&other));
    let fl = check_ok!(fsutil::fcntl_getfl(&a));
    check_ok!(fsutil::fcntl_setfl(&a, fl | libc::O_APPEND));
    check_ok!(a.write_all(b"X"));
    check_ok!(b.seek(SeekFrom::Start(0)));
    check_ok!(b.write_all(b"Y"));

    let mut st = check_ok!(fsutil::open_stat(&other));
    let mut buf = [0u8; 4];
    check_errno!(st.read(&mut buf), fsutil::EBADF, "read O_STAT");
    check_errno!(st.write_all(b"x"), fsutil::EBADF, "write O_STAT");

    let mut sl = check_ok!(fsutil::open_symlink(&link));
    check_errno!(sl.write_all(b"x"), fsutil::EBADF, "write symlink fd");

    let dir = check_ok!(fsutil::open_dir(&dirp));
    let fl = check_ok!(fsutil::fcntl_getfl(&dir));
    check_ok!(fsutil::fcntl_setfl(&dir, fl));
    let _ = dir;
    Outcome::Pass
}
