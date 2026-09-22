use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};

use libredox::flag;

use crate::config::FIXTURES_DIR;
use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok,
    check_out, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func open_known_file,
        func open_nested_dir,
        func create_new_file,
        func create_and_reopen,
        func create_excl_new,
        func open_o_stat_file,
        func open_o_stat_dir,
        func openat_relative_file,
        func openat_relative_create,
        func openat_from_ostat_dir,
        func openat_dot,
        func multiple_handles_same_file,
        func open_dup,
        edge edge_open_errnos,
        edge edge_scheme_dup_unsupported,
    ]
}

fn open_known_file(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let data = check_ok!(fsutil::read_to_vec(&ctx.fixtures().join("known.txt")));
    check!(
        data.windows(crate::config::KNOWN_MARKER.len())
            .any(|w| w == crate::config::KNOWN_MARKER.as_bytes()),
        "unexpected known.txt: {data:?}"
    );
    Outcome::Pass
}

fn open_nested_dir(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    check_ok!(fsutil::open_dir(&ctx.fixtures().join("nested")));
    Outcome::Pass
}

fn create_new_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_new_file.txt");
    let mut f = check_ok!(fsutil::create_rw(&path));
    check_ok!(f.write_all(b"hello-create"));
    check_ok!(f.sync_all());
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"hello-create");
    Outcome::Pass
}

fn create_and_reopen(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_reopen.txt");
    check_ok!(fsutil::write_all_path(&path, b"payload-v1"));
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"payload-v1");
    Outcome::Pass
}

fn create_excl_new(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_excl_new.txt");
    let mut f = check_ok!(fsutil::create_excl(&path));
    check_ok!(f.write_all(b"exclusive"));
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"exclusive");
    Outcome::Pass
}

fn open_o_stat_file(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let f = check_ok!(fsutil::open_stat(&ctx.fixtures().join("known.txt")));
    let p = check_ok!(fsutil::fpath(&f));
    check!(p.contains("known.txt"), "fpath unexpected: {p}");
    Outcome::Pass
}

fn open_o_stat_dir(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let f = check_ok!(fsutil::open_stat(&ctx.fixtures()));
    let p = check_ok!(fsutil::fpath(&f));
    check!(p.contains(FIXTURES_DIR), "fpath unexpected: {p}");
    Outcome::Pass
}

fn openat_relative_file(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let dir = check_ok!(fsutil::open_dir(&ctx.fixtures()));
    let mut f = check_ok!(fsutil::openat_ro(&dir, "known.txt"));
    let mut buf = String::new();
    check_ok!(std::io::Read::read_to_string(&mut f, &mut buf));
    check!(
        buf.contains(crate::config::KNOWN_MARKER),
        "content mismatch: {buf:?}"
    );
    Outcome::Pass
}

fn openat_relative_create(ctx: &TestCtx) -> Outcome {
    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    let path = ctx.work("openat_create.txt");
    let name = fsutil::base_name(&path);
    let mut f = check_ok!(fsutil::openat_create(&dir, name));
    check_ok!(f.write_all(b"via-openat"));
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"via-openat");
    Outcome::Pass
}

fn openat_from_ostat_dir(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    // StatOnly openat has to use the inode's mode, not the handle's.
    let st = check_ok!(fsutil::open_stat(&ctx.fixtures()));
    let mut f = check_ok!(fsutil::openat_ro(&st, "known.txt"));
    let mut buf = String::new();
    check_ok!(std::io::Read::read_to_string(&mut f, &mut buf));
    check!(
        buf.contains(crate::config::KNOWN_MARKER),
        "content mismatch: {buf:?}"
    );
    Outcome::Pass
}

fn openat_dot(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let dir = check_ok!(fsutil::open_dir(&ctx.fixtures()));
    let dot = check_ok!(fsutil::openat_dir(&dir, "."));
    let p = check_ok!(fsutil::fpath(&dot));
    check!(p.contains(FIXTURES_DIR), "fpath of '.' unexpected: {p}");
    Outcome::Pass
}

fn multiple_handles_same_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("multi_handle.txt");
    check_ok!(fsutil::write_all_path(&path, b"shared"));
    let f1 = check_ok!(fsutil::open_ro(&path));
    let f2 = check_ok!(fsutil::open_ro(&path));
    let f3 = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::fpath(&f1));
    drop(f2);
    check_ok!(fsutil::fpath(&f3));
    drop(f1);
    drop(f3);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"shared");
    Outcome::Pass
}

fn open_dup(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("open_dup.bin");
    check_ok!(fsutil::write_all_path(&path, b"AAAA"));
    let f = check_ok!(fsutil::open_rw(&path));
    let dup_fd = unsafe { libc::dup(f.as_raw_fd()) };
    check!(
        dup_fd >= 0,
        "dup failed: {}",
        std::io::Error::last_os_error()
    );
    let mut g = unsafe { File::from_raw_fd(dup_fd) };
    check_ok!(g.write_all(b"BB"));
    drop(g);
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"BBAA");
    Outcome::Pass
}

fn edge_open_errnos(ctx: &TestCtx) -> Outcome {
    check_errno!(
        fsutil::open_ro(&ctx.root().join(".__no_such_file__")),
        fsutil::ENOENT,
        "missing file"
    );
    check_errno!(
        fsutil::open_flags(ctx.root(), flag::O_WRONLY, 0),
        fsutil::EISDIR,
        "O_WRONLY on dir"
    );
    let mut dir = check_ok!(fsutil::open_dir(ctx.root()));
    check_errno!(dir.write(b"x"), fsutil::EISDIR, "write dir");
    check_errno!(dir.read(&mut [0u8; 1]), fsutil::EISDIR, "read dir");
    check_errno!(
        fsutil::open_flags(ctx.root(), flag::O_RDONLY, 0),
        fsutil::EISDIR,
        "open dir without O_DIRECTORY"
    );

    let file = ctx.work("edge_enotdir.txt");
    check_ok!(fsutil::write_all_path(&file, b"x"));
    check_errno!(fsutil::open_dir(&file), fsutil::ENOTDIR, "open file as dir");
    let fd = check_ok!(fsutil::open_rw(&file));
    check_errno!(
        fsutil::openat_dir(&fd, "x"),
        fsutil::ENOTDIR,
        "openat from file fd"
    );

    check_errno!(
        fsutil::create_excl(&file),
        fsutil::EEXIST,
        "O_CREAT|O_EXCL existing"
    );
    let mut f = check_ok!(fsutil::open_flags(
        &file,
        flag::O_RDWR | flag::O_CREAT,
        0o644
    ));
    check_ok!(f.write_all(b"keep"));
    drop(f);
    check_ok!(fsutil::open_ro(&file));

    let spaced = ctx.work("name with spaces.txt");
    check_ok!(fsutil::write_all_path(&spaced, b"spaces-ok"));
    check_bytes!(check_ok!(fsutil::read_to_vec(&spaced)), b"spaces-ok");

    let storm = ctx.work("edge_open_close.txt");
    check_ok!(fsutil::write_all_path(&storm, b"stable"));
    for i in 0..64 {
        drop(check_ok!(fsutil::open_ro(&storm), "open {i}"));
    }
    check_bytes!(check_ok!(fsutil::read_to_vec(&storm)), b"stable");
    Outcome::Pass
}

fn edge_scheme_dup_unsupported(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_dup_eopnotsupp.txt");
    check_ok!(fsutil::write_all_path(&path, b"test"));
    let f = check_ok!(fsutil::open_ro(&path));
    check_errno!(
        fsutil::scheme_dup(&f, b"subpath"),
        fsutil::EOPNOTSUPP,
        "scheme dup"
    );
    Outcome::Pass
}
