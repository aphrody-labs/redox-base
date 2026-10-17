use std::io::Write;

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_eq, check_errno, check_errno_any, check_ok,
    fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func mkdir_new,
        func mkdir_nested,
        func mkdir_via_openat,
        func mkdir_existing_without_excl,
        func mkdir_o_stat_create,
        func mkdir_o_stat_write,
        func mkdir_mode,
        edge edge_mkdir_errnos,
        edge edge_mkdir_empty_name,
    ]
}

fn mkdir_new(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("mkdir_new");
    let name = fsutil::base_name(&path).to_owned();
    let dir = check_ok!(fsutil::mkdir(&path));
    let p = check_ok!(fsutil::fpath(&dir));
    check!(p.contains(&name), "fpath unexpected: {p}");
    drop(dir);

    check_ok!(fsutil::open_dir(&path));
    let meta = check_ok!(std::fs::metadata(&path));
    check!(meta.is_dir(), "expected directory");

    let names = check_ok!(fsutil::read_dir_names(&path));
    let extra: Vec<_> = names
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect();
    check!(extra.is_empty(), "new dir not empty: {extra:?}");

    let parent_names = check_ok!(fsutil::read_dir_names(ctx.root()));
    check!(
        parent_names.iter().any(|n| n == &name),
        "parent readdir missing `{name}`: {parent_names:?}"
    );
    Outcome::Pass
}

fn mkdir_nested(ctx: &TestCtx) -> Outcome {
    let parent = ctx.work("mkdir_nest");
    check_ok!(fsutil::make_dir(&parent));
    let child = parent.join("child");
    check_ok!(fsutil::make_dir(&child));
    let file = child.join("leaf.txt");
    check_ok!(fsutil::write_all_path(&file, b"nested-ok"));
    let got = check_ok!(fsutil::read_to_vec(&file));
    check_eq!(got.as_slice(), b"nested-ok");
    Outcome::Pass
}

fn mkdir_via_openat(ctx: &TestCtx) -> Outcome {
    let parent = check_ok!(fsutil::open_dir(ctx.root()));
    let path = ctx.work("mkdir_openat");
    let name = fsutil::base_name(&path);
    let dir = check_ok!(fsutil::openat_mkdir(&parent, name));
    drop(dir);
    check_ok!(fsutil::open_dir(&path));
    Outcome::Pass
}

fn mkdir_existing_without_excl(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("mkdir_noexcl");
    check_ok!(fsutil::make_dir(&path));
    let again = check_ok!(fsutil::mkdir_noexcl(&path));
    drop(again);
    check_ok!(fsutil::open_dir(&path));
    Outcome::Pass
}

fn mkdir_o_stat_create(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("mkdir_ostat");
    let name = fsutil::base_name(&path).to_owned();
    let st = check_ok!(fsutil::open_flags(
        &path,
        fsutil::O_STAT | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL,
        0o755,
    ));
    let p = check_ok!(fsutil::fpath(&st));
    check!(
        p.contains(&name),
        "O_STAT fpath was parent, not new dir: {p}"
    );
    let mut f = check_ok!(fsutil::openat_create(&st, "inside.txt"));
    check_ok!(f.write_all(b"via-ostat-dir"));
    drop(f);
    drop(st);
    let got = check_ok!(fsutil::read_to_vec(&path.join("inside.txt")));
    check_eq!(got.as_slice(), b"via-ostat-dir");
    Outcome::Pass
}

fn mkdir_o_stat_write(ctx: &TestCtx) -> Outcome {
    // Write access mode applies to the I/O open. O_STAT does not take that open,
    // so the directory is still created and the fd is stat-only.
    let path = ctx.work("mkdir_ostat_rdwr");
    let mut st = check_ok!(fsutil::open_flags(
        &path,
        fsutil::O_STAT | flag::O_RDWR | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL,
        0o755,
    ));
    check_errno!(st.write_all(b"x"), fsutil::EBADF, "write O_STAT mkdir");
    drop(st);
    check_ok!(fsutil::open_dir(&path));
    Outcome::Pass
}

fn mkdir_mode(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("mkdir_mode");
    check_ok!(fsutil::open_flags(
        &path,
        flag::O_RDONLY | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL,
        0o700,
    ));
    check_eq!(check_ok!(fsutil::mode_of(&path)), 0o700);
    check_eq!(check_ok!(fsutil::type_of(&path)), fsutil::MODE_DIR);
    Outcome::Pass
}

fn edge_mkdir_errnos(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_mkdir_excl");
    check_ok!(fsutil::make_dir(&path));
    check_errno!(fsutil::mkdir(&path), fsutil::EEXIST, "O_EXCL mkdir");

    check_errno!(
        fsutil::mkdir(&ctx.work("edge_mkdir_noparent").join("child")),
        fsutil::ENOENT,
        "mkdir missing parent"
    );

    let file = ctx.work("edge_mkdir_notdir.txt");
    check_ok!(fsutil::write_all_path(&file, b"not-a-dir"));
    check_errno!(
        fsutil::mkdir(&file.join("child")),
        fsutil::ENOTDIR,
        "mkdir through file"
    );

    for (flags, what) in [
        (
            flag::O_WRONLY | flag::O_CREAT | flag::O_DIRECTORY,
            "O_WRONLY mkdir",
        ),
        (
            flag::O_RDWR | flag::O_CREAT | flag::O_DIRECTORY,
            "O_RDWR mkdir",
        ),
    ] {
        let p = ctx.work(&format!("edge_mkdir_{what}"));
        check_errno!(
            fsutil::open_flags(&p, flags, 0o755),
            fsutil::EISDIR,
            "{what}"
        );
        check_errno!(
            fsutil::open_dir(&p),
            fsutil::ENOENT,
            "{what} must not create the dir"
        );
    }
    Outcome::Pass
}

fn edge_mkdir_empty_name(ctx: &TestCtx) -> Outcome {
    let root = check_ok!(fsutil::open_dir(ctx.root()));
    let flags = (flag::O_RDONLY | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL) | 0o755;
    check_errno_any!(
        fsutil::openat(&root, "", flags),
        [fsutil::EEXIST, fsutil::ENOENT, fsutil::EINVAL],
        "mkdir empty name"
    );
    Outcome::Pass
}
