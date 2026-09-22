use std::io::{Read, Seek, SeekFrom, Write};

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_errno_any,
    check_ok, check_out, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func unlink_new_file,
        func unlink_nested_file,
        func unlink_via_openat,
        func unlink_open_file_keeps_io,
        func unlink_stat_only,
        func unlink_then_recreate,
        func unlink_readdir_gone,
        func unlink_host_created,
        func unlink_hardlink_other_name,
        func rmdir_empty,
        func rmdir_nested_tree,
        func rmdir_via_openat,
        func rmdir_open_dir_name_gone,
        func rmdir_host_empty,
        edge edge_unlink_errnos,
        edge edge_rmdir_errnos,
        edge edge_unlinkat_special_names,
        edge edge_name_max,
    ]
}

fn unlink_new_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("ul_new.txt");
    let name = fsutil::base_name(&path).to_owned();
    check_ok!(fsutil::write_all_path(&path, b"bye"));
    check_ok!(fsutil::unlink(&path));
    check!(fsutil::gone(&path), "name still opens after unlink");
    check!(
        !check_ok!(fsutil::listed(ctx.root(), &name)),
        "unlinked `{name}` still in parent readdir"
    );
    Outcome::Pass
}

fn unlink_nested_file(ctx: &TestCtx) -> Outcome {
    let dir = ctx.work("ul_nest");
    check_ok!(fsutil::make_dir(&dir));
    let file = dir.join("leaf.txt");
    check_ok!(fsutil::write_all_path(&file, b"nested-unlink"));
    check_ok!(fsutil::unlink(&file));
    check!(fsutil::gone(&file), "leaf still present");
    check!(
        !check_ok!(fsutil::listed(&dir, "leaf.txt")),
        "leaf.txt still in nested readdir"
    );
    check_ok!(fsutil::rmdir(&dir));
    Outcome::Pass
}

fn unlink_via_openat(ctx: &TestCtx) -> Outcome {
    let parent = check_ok!(fsutil::open_dir(ctx.root()));
    let path = ctx.work("ul_openat.txt");
    check_ok!(fsutil::write_all_path(&path, b"via-openat"));
    check_ok!(fsutil::unlinkat_file(&parent, fsutil::base_name(&path)));
    check!(fsutil::gone(&path), "openat-unlinked name still present");
    Outcome::Pass
}

fn unlink_open_file_keeps_io(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("ul_open.txt");
    let mut f = check_ok!(fsutil::create_rw(&path));
    check_ok!(f.write_all(b"still-here"));
    check_ok!(f.sync_all());
    check_ok!(fsutil::unlink(&path));
    check!(fsutil::gone(&path), "name should be gone");
    check_ok!(f.seek(SeekFrom::Start(0)));
    let mut buf = Vec::new();
    check_ok!(f.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"still-here");
    check_ok!(f.write_all(b"+"));
    drop(f);
    check!(fsutil::gone(&path), "name reappeared after last close");
    Outcome::Pass
}

fn unlink_stat_only(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("ul_ostat.txt");
    check_ok!(fsutil::write_all_path(&path, b"stat-then-unlink"));
    let st = check_ok!(fsutil::open_stat(&path));
    check_ok!(fsutil::unlink(&path));
    check!(fsutil::gone(&path), "name still present");
    drop(st);
    check!(fsutil::gone(&path), "name reappeared after StatOnly close");
    Outcome::Pass
}

fn unlink_then_recreate(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("ul_recreate.txt");
    check_ok!(fsutil::write_all_path(&path, b"v1"));
    check_ok!(fsutil::unlink(&path));
    check_ok!(fsutil::write_all_path(&path, b"v2"));
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"v2");
    check_ok!(fsutil::unlink(&path));
    Outcome::Pass
}

fn unlink_readdir_gone(ctx: &TestCtx) -> Outcome {
    let dir = ctx.work("ul_readdir");
    check_ok!(fsutil::make_dir(&dir));
    check_ok!(fsutil::write_all_path(&dir.join("keep.txt"), b"keep"));
    check_ok!(fsutil::write_all_path(&dir.join("drop.txt"), b"drop"));
    check_ok!(fsutil::unlink(&dir.join("drop.txt")));
    check!(
        check_ok!(fsutil::listed(&dir, "keep.txt")),
        "keep.txt missing"
    );
    check!(
        !check_ok!(fsutil::listed(&dir, "drop.txt")),
        "drop.txt still listed"
    );
    check_ok!(fsutil::unlink(&dir.join("keep.txt")));
    check_ok!(fsutil::rmdir(&dir));
    Outcome::Pass
}

fn unlink_host_created(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let path = ctx.fixtures().join("deletable/once_file.txt");
    check_out!(fsutil::skip_if_missing(&path, "host once_file.txt"));
    check_ok!(fsutil::unlink(&path));
    check!(fsutil::gone(&path), "host file still present");
    Outcome::Pass
}

fn unlink_hardlink_other_name(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let foo = ctx.fixtures().join("deletable/a/foo");
    let bar = ctx.fixtures().join("deletable/b/bar");
    check_out!(fsutil::skip_if_missing(&foo, "hard-link a/foo"));
    check_out!(fsutil::skip_if_missing(&bar, "hard-link b/bar"));
    let via_bar = check_ok!(fsutil::read_to_vec(&bar));
    check_ok!(fsutil::unlink(&foo));
    check!(fsutil::gone(&foo), "a/foo still present");
    let still = check_ok!(fsutil::read_to_vec(&bar));
    check_eq!(still.as_slice(), via_bar.as_slice());
    check!(
        !check_ok!(fsutil::listed(&ctx.fixtures().join("deletable/a"), "foo")),
        "a/foo still listed"
    );
    check!(
        check_ok!(fsutil::listed(&ctx.fixtures().join("deletable/b"), "bar")),
        "b/bar disappeared"
    );
    Outcome::Pass
}

fn rmdir_empty(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("rm_empty");
    let name = fsutil::base_name(&path).to_owned();
    check_ok!(fsutil::make_dir(&path));
    check_ok!(fsutil::rmdir(&path));
    check!(fsutil::gone(&path), "dir still opens after rmdir");
    check!(
        !check_ok!(fsutil::listed(ctx.root(), &name)),
        "rmdir'd `{name}` still in parent readdir"
    );
    Outcome::Pass
}

fn rmdir_nested_tree(ctx: &TestCtx) -> Outcome {
    let parent = ctx.work("rm_tree");
    check_ok!(fsutil::make_dir(&parent));
    let child = parent.join("child");
    check_ok!(fsutil::make_dir(&child));
    let leaf = child.join("leaf.txt");
    check_ok!(fsutil::write_all_path(&leaf, b"x"));
    check_errno!(
        fsutil::rmdir(&parent),
        fsutil::ENOTEMPTY,
        "rmdir parent with child"
    );
    check_ok!(fsutil::unlink(&leaf));
    check_ok!(fsutil::rmdir(&child));
    check_ok!(fsutil::rmdir(&parent));
    check!(fsutil::gone(&parent), "parent still present");
    Outcome::Pass
}

fn rmdir_via_openat(ctx: &TestCtx) -> Outcome {
    let parent = check_ok!(fsutil::open_dir(ctx.root()));
    let path = ctx.work("rm_openat");
    check_ok!(fsutil::make_dir(&path));
    check_ok!(fsutil::unlinkat_dir(&parent, fsutil::base_name(&path)));
    check!(fsutil::gone(&path), "openat-rmdir name still present");
    Outcome::Pass
}

fn rmdir_open_dir_name_gone(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("rm_open_dir");
    let dir = check_ok!(fsutil::mkdir(&path));
    check_ok!(fsutil::rmdir(&path));
    check!(fsutil::gone(&path), "name still present");
    let p = check_ok!(fsutil::fpath(&dir));
    check!(!p.is_empty(), "fpath empty on live fd");
    drop(dir);
    check!(fsutil::gone(&path), "name reappeared after last close");
    Outcome::Pass
}

fn rmdir_host_empty(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let path = ctx.fixtures().join("deletable/once_dir");
    check_out!(fsutil::skip_if_missing(&path, "host once_dir"));
    check_ok!(fsutil::rmdir(&path));
    check!(fsutil::gone(&path), "host dir still present");
    Outcome::Pass
}

fn edge_unlink_errnos(ctx: &TestCtx) -> Outcome {
    check_errno!(
        fsutil::unlink(&ctx.root().join(".__no_unlink__")),
        fsutil::ENOENT,
        "unlink missing"
    );

    let dir = ctx.work("edge_ul_dir");
    check_ok!(fsutil::make_dir(&dir));
    check_errno!(fsutil::unlink(&dir), fsutil::EISDIR, "unlink dir");
    check_ok!(fsutil::open_dir(&dir));
    check_ok!(fsutil::rmdir(&dir));

    let file = ctx.work("edge_ul_notdir.txt");
    check_ok!(fsutil::write_all_path(&file, b"not-a-dir"));
    check_errno!(
        fsutil::unlink(&file.join("child")),
        fsutil::ENOTDIR,
        "unlink through file"
    );

    let from = ctx.work("edge_ul_fromfile.txt");
    let fd = check_ok!(fsutil::create_rw(&from));
    check_errno!(
        fsutil::unlinkat_file(&fd, "x"),
        fsutil::ENOTDIR,
        "unlinkat from file"
    );
    drop(fd);
    check_ok!(fsutil::unlink(&from));

    let twice = ctx.work("edge_ul_twice.txt");
    check_ok!(fsutil::write_all_path(&twice, b"x"));
    check_ok!(fsutil::unlink(&twice));
    check_errno!(fsutil::unlink(&twice), fsutil::ENOENT, "second unlink");

    check_errno!(
        fsutil::unlink(&ctx.work("edge_ul_noparent").join("x.txt")),
        fsutil::ENOENT,
        "unlink under missing dir"
    );

    let spaced = ctx.work("edge ul spaces.txt");
    check_ok!(fsutil::write_all_path(&spaced, b"spaces"));
    check_ok!(fsutil::unlink(&spaced));
    check!(fsutil::gone(&spaced), "spaced name still present");
    Outcome::Pass
}

fn edge_rmdir_errnos(ctx: &TestCtx) -> Outcome {
    check_errno!(
        fsutil::rmdir(&ctx.root().join(".__no_rmdir__")),
        fsutil::ENOENT,
        "rmdir missing"
    );

    let file = ctx.work("edge_rm_file.txt");
    check_ok!(fsutil::write_all_path(&file, b"not-a-dir"));
    check_errno!(fsutil::rmdir(&file), fsutil::ENOTDIR, "rmdir file");
    check_ok!(fsutil::open_ro(&file));
    check_ok!(fsutil::unlink(&file));

    let dir = ctx.work("edge_rm_full");
    check_ok!(fsutil::make_dir(&dir));
    check_ok!(fsutil::write_all_path(&dir.join("x.txt"), b"x"));
    check_errno!(fsutil::rmdir(&dir), fsutil::ENOTEMPTY, "rmdir nonempty");
    check_ok!(fsutil::unlink(&dir.join("x.txt")));
    check_ok!(fsutil::rmdir(&dir));

    let parent = check_ok!(fsutil::open_dir(ctx.root()));
    let flag_file = ctx.work("edge_rmdir_file_flag.txt");
    check_ok!(fsutil::write_all_path(&flag_file, b"file"));
    check_errno!(
        fsutil::unlinkat_dir(&parent, fsutil::base_name(&flag_file)),
        fsutil::ENOTDIR,
        "AT_REMOVEDIR on file"
    );
    check_ok!(fsutil::unlink(&flag_file));

    let twice = ctx.work("edge_rm_twice");
    check_ok!(fsutil::make_dir(&twice));
    check_ok!(fsutil::rmdir(&twice));
    check_errno!(fsutil::rmdir(&twice), fsutil::ENOENT, "second rmdir");
    Outcome::Pass
}

fn edge_unlinkat_special_names(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_ul_special");
    let dir = check_ok!(fsutil::mkdir(&path));

    // Linux unlink(".") is EISDIR. Relibc may fold the name first; either way
    // the scheme still sees a directory.
    for name in [".", ".."] {
        check_errno_any!(
            fsutil::unlinkat(&dir, name, 0),
            [fsutil::EISDIR, fsutil::EINVAL],
            "unlink {name}"
        );
        check_ok!(
            fsutil::open_dir(&path),
            "unlink {name}: dir must still exist"
        );
    }

    for (flag, what) in [(0, "unlink empty"), (fsutil::AT_REMOVEDIR, "rmdir empty")] {
        check_errno_any!(
            fsutil::unlinkat(&dir, "", flag),
            [fsutil::ENOENT, fsutil::EINVAL],
            "{what}"
        );
        check_ok!(fsutil::open_dir(&path), "{what}: dir must still exist");
    }

    // POSIX wants EINVAL; after relibc fold this is rmdir of the (non-empty) parent.
    check_errno_any!(
        fsutil::unlinkat(&dir, "..", fsutil::AT_REMOVEDIR),
        [fsutil::EINVAL, fsutil::ENOTEMPTY, fsutil::EISDIR],
        "rmdir .."
    );
    check_ok!(fsutil::open_dir(&path), "rmdir ..: dir must still exist");
    check_ok!(
        fsutil::open_dir(ctx.root()),
        "rmdir ..: parent must still exist"
    );

    drop(dir);
    check_ok!(fsutil::rmdir(&path));

    // Relibc folds rmdir(".") into rmdir of this empty dir, so it can succeed.
    let dot = ctx.work("edge_ul_special_dot");
    let dotfd = check_ok!(fsutil::mkdir(&dot));
    match fsutil::unlinkat(&dotfd, ".", fsutil::AT_REMOVEDIR) {
        Err(e)
            if fsutil::is_errno(&e, fsutil::EINVAL) || fsutil::is_errno(&e, fsutil::ENOTEMPTY) =>
        {
            check_ok!(fsutil::open_dir(&dot), "rmdir .: dir must still exist");
            drop(dotfd);
            check_ok!(fsutil::rmdir(&dot));
            Outcome::Pass
        }
        Err(e) => Outcome::Fail(format!("rmdir .: unexpected {e:?}")),
        Ok(()) => {
            drop(dotfd);
            check!(fsutil::gone(&dot), "rmdir . succeeded but name remains");
            Outcome::PassNote("relibc folded rmdir(\".\") into rmdir of the directory".into())
        }
    }
}

fn edge_name_max(ctx: &TestCtx) -> Outcome {
    let dirp = ctx.work("edge_namemax");
    let dir = check_ok!(fsutil::mkdir(&dirp));
    let n255 = "n".repeat(255);
    let n256 = "n".repeat(256);

    check_ok!(fsutil::openat_create(&dir, &n255));
    drop(check_ok!(fsutil::openat_ro(&dir, &n255)));
    check_ok!(fsutil::unlinkat_file(&dir, &n255));
    drop(check_ok!(fsutil::openat_mkdir(&dir, &n255)));
    check_ok!(fsutil::unlinkat_dir(&dir, &n255));

    for (res, what) in [
        (fsutil::openat_create(&dir, &n256).map(drop), "CREATE 256"),
        (fsutil::openat_ro(&dir, &n256).map(drop), "LOOKUP 256"),
        (fsutil::openat_mkdir(&dir, &n256).map(drop), "MKDIR 256"),
        (fsutil::unlinkat_file(&dir, &n256), "UNLINK 256"),
        (fsutil::unlinkat_dir(&dir, &n256), "RMDIR 256"),
    ] {
        check_errno!(res, fsutil::ENAMETOOLONG, "{what}");
    }
    drop(dir);
    check_ok!(fsutil::rmdir(&dirp));
    Outcome::Pass
}
