use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok,
    check_out, check_str, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func symlink_readlink_relative => |ctx| readlink_is(ctx, "to_file", "target.txt"),
        func symlink_readlink_dot_relative => |ctx| readlink_is(ctx, "to_file_dot", "./target.txt"),
        func symlink_o_symlink_read,
        func symlink_follow_file => |ctx| follow_reads(ctx, "to_file", b"link-target\n"),
        func symlink_follow_dot_relative => |ctx| follow_reads(ctx, "to_file_dot", b"link-target\n"),
        func symlink_follow_dotdot,
        func symlink_follow_dir,
        func symlink_follow_intermediate,
        func symlink_lstat,
        func symlink_ostat_no_follow,
        func symlink_follow_fpath,
        func symlink_o_symlink_fpath,
        func symlink_chain_follow => |ctx| follow_reads(ctx, "chain_a", b"link-target\n"),
        func symlink_chain_readlink => |ctx| readlink_is(ctx, "chain_a", "chain_b"),
        func symlink_readdir,
        func symlink_openat,
        func symlink_abs_from_root,
        func symlink_name_with_spaces,
        func symlink_create,
        func symlink_create_follow,
        func symlink_create_lstat,
        func symlink_create_readdir,
        func symlink_create_unlink_keeps_target,
        edge edge_symlink_nofollow => |ctx| open_fails(ctx, "to_file", open_nofollow, fsutil::ELOOP, "O_NOFOLLOW on symlink"),
        edge edge_symlink_o_symlink_on_file,
        edge edge_symlink_o_symlink_on_dir,
        edge edge_symlink_dangling_follow => |ctx| open_fails(ctx, "dangling", fsutil::open_ro, fsutil::ENOENT, "follow dangling"),
        edge edge_symlink_dangling_readlink => |ctx| readlink_is(ctx, "dangling", "no_such_name"),
        edge edge_symlink_follow_dir_as_file => |ctx| open_fails(ctx, "to_dir", fsutil::open_ro, fsutil::EISDIR, "follow dir symlink without O_DIRECTORY"),
        edge edge_symlink_o_directory_on_file_link => |ctx| open_fails(ctx, "to_file", fsutil::open_dir, fsutil::ENOTDIR, "O_DIRECTORY on file symlink"),
        edge edge_symlink_loop => |ctx| open_fails(ctx, "loop_a", fsutil::open_ro, fsutil::ELOOP, "follow loop_a → loop_b → loop_a"),
        edge edge_symlink_missing_readlink,
        edge edge_unlink_symlink_keeps_target,
        edge edge_o_symlink_read_offset,
        edge edge_symlink_create_empty_target,
        edge edge_symlink_create_existing_file,
        edge edge_symlink_create_existing_symlink,
        edge edge_symlink_create_close_without_write,
        edge edge_symlink_create_write_offset,
        edge edge_symlink_create_second_write,
        edge edge_symlink_create_fstat_before_write,
        edge edge_symlink_create_o_directory,
        edge edge_symlink_create_rdonly_write,
    ]
}

fn links(ctx: &TestCtx) -> Result<PathBuf, Outcome> {
    ctx.require_fixtures()?;
    let dir = ctx.fixtures().join("links");
    if fsutil::is_dir(&dir) {
        Ok(dir)
    } else {
        Err(Outcome::Skip(format!(
            "fixtures/links missing at {} - re-run scripts/gen-fixtures.sh",
            dir.display()
        )))
    }
}

fn require_link(ctx: &TestCtx, name: &str) -> Result<PathBuf, Outcome> {
    let dir = links(ctx)?;
    let path = dir.join(name);
    match fsutil::is_symlink(&path) {
        Ok(true) => Ok(path),
        Ok(false) => Err(Outcome::Skip(format!(
            "{name} exists but is not a symlink - re-run scripts/gen-fixtures.sh"
        ))),
        Err(e) if fsutil::is_errno(&e, fsutil::ENOENT) => Err(Outcome::Skip(format!(
            "{name} missing - re-run scripts/gen-fixtures.sh"
        ))),
        Err(e) => Err(Outcome::Fail(format!("lstat {}: {e}", path.display()))),
    }
}

fn readlink_is(ctx: &TestCtx, name: &str, want: &str) -> Outcome {
    let path = check_out!(require_link(ctx, name));
    check_str!(check_ok!(fsutil::readlink(&path)), want);
    Outcome::Pass
}

fn follow_reads(ctx: &TestCtx, name: &str, want: &[u8]) -> Outcome {
    let path = check_out!(require_link(ctx, name));
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), want);
    Outcome::Pass
}

fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    fsutil::open_flags(path, flag::O_RDONLY | fsutil::O_NOFOLLOW, 0)
}

fn open_fails(
    ctx: &TestCtx,
    name: &str,
    open: fn(&Path) -> std::io::Result<std::fs::File>,
    errno: i32,
    what: &str,
) -> Outcome {
    let path = check_out!(require_link(ctx, name));
    check_errno!(open(&path), errno, "{what}");
    Outcome::Pass
}

fn symlink_o_symlink_read(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file_dot"));
    let mut f = check_ok!(fsutil::open_symlink(&path));
    let mut buf = Vec::new();
    check_ok!(f.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"./target.txt");
    Outcome::Pass
}

fn symlink_follow_dotdot(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_nested"));
    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(
        got.windows(b"small".len()).any(|w| w == b"small"),
        "followed nested content unexpected: {got:?}"
    );
    Outcome::Pass
}

fn symlink_follow_dir(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_dir"));
    let dir = check_ok!(fsutil::open_dir(&path));
    let p = check_ok!(fsutil::fpath(&dir));
    check!(p.contains("nested"), "followed dir fpath unexpected: {p}");
    Outcome::Pass
}

fn symlink_follow_intermediate(ctx: &TestCtx) -> Outcome {
    let dir = check_out!(links(ctx));
    check_out!(require_link(ctx, "to_dir"));
    let through = dir.join("to_dir").join("a");
    let child = check_ok!(fsutil::open_dir(&through));
    let p = check_ok!(fsutil::fpath(&child));
    check!(
        p.contains("nested") && p.contains("a"),
        "intermediate follow fpath unexpected: {p}"
    );
    Outcome::Pass
}

fn symlink_lstat(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file_dot"));
    check!(
        check_ok!(fsutil::is_symlink(&path)),
        "to_file_dot should be a symlink"
    );
    check_eq!(
        check_ok!(fsutil::lstat(&path)).st_size as u64,
        "./target.txt".len() as u64
    );
    Outcome::Pass
}

fn symlink_ostat_no_follow(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file"));
    let f = check_ok!(fsutil::open_stat(&path));
    let p = check_ok!(fsutil::fpath(&f));
    check!(p.contains("to_file"), "O_STAT must not follow; fpath={p}");
    Outcome::Pass
}

fn symlink_follow_fpath(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file"));
    let f = check_ok!(fsutil::open_ro(&path));
    let p = check_ok!(fsutil::fpath(&f));
    check!(
        p.contains("target.txt"),
        "followed fpath should be the target, got {p}"
    );
    check_eq!(p.rsplit('/').next().unwrap_or(""), "target.txt");
    Outcome::Pass
}

fn symlink_o_symlink_fpath(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file"));
    let f = check_ok!(fsutil::open_symlink(&path));
    let p = check_ok!(fsutil::fpath(&f));
    check_eq!(p.rsplit('/').next().unwrap_or(""), "to_file");
    Outcome::Pass
}

fn symlink_readdir(ctx: &TestCtx) -> Outcome {
    let dir = check_out!(links(ctx));
    let names = check_ok!(fsutil::read_dir_names(&dir));
    for expected in [
        "to_file",
        "to_file_dot",
        "target.txt",
        "dangling",
        "chain_a",
    ] {
        check!(
            names.iter().any(|n| n == expected),
            "links/ missing `{expected}`; got {names:?}"
        );
    }
    Outcome::Pass
}

fn symlink_openat(ctx: &TestCtx) -> Outcome {
    let dirp = check_out!(links(ctx));
    check_out!(require_link(ctx, "to_file"));
    let dir = check_ok!(fsutil::open_dir(&dirp));
    let mut f = check_ok!(fsutil::openat_ro(&dir, "to_file"));
    let mut buf = Vec::new();
    check_ok!(f.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"link-target\n");
    Outcome::Pass
}

fn symlink_abs_from_root(ctx: &TestCtx) -> Outcome {
    // Leading `/` is the virtio-fs share root, not host `/` and not `--root`.
    let payload = b"abs-from-fuse-root\n";
    let target_file = ctx.work("abs_tgt.txt");
    check_ok!(fsutil::write_all_path(&target_file, payload));
    let f = check_ok!(fsutil::open_ro(&target_file));
    let fpath = check_ok!(fsutil::fpath(&f));
    drop(f);

    let share_rel = check_out!(share_rel_from_fpath(&fpath));
    let abs_target = format!("/{share_rel}");
    let link = ctx.work("abs_link");
    check_ok!(fsutil::symlink(&abs_target, &link));
    check_str!(check_ok!(fsutil::readlink(&link)), abs_target.as_str());
    check_bytes!(check_ok!(fsutil::read_to_vec(&link)), payload);
    Outcome::Pass
}

fn share_rel_from_fpath(fpath: &str) -> Result<String, Outcome> {
    let rest = fpath
        .strip_prefix("/scheme/")
        .ok_or_else(|| Outcome::Fail(format!("fpath is not a scheme path: {fpath}")))?;
    let share_rel = rest.split_once('/').map(|(_, r)| r).unwrap_or("");
    if share_rel.is_empty() {
        return Err(Outcome::Fail(format!(
            "fpath has no path after scheme name: {fpath}"
        )));
    }
    Ok(share_rel.to_owned())
}

fn symlink_name_with_spaces(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "with spaces"));
    check_str!(check_ok!(fsutil::readlink(&path)), "target.txt");
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"link-target\n");
    Outcome::Pass
}

fn edge_symlink_o_symlink_on_file(ctx: &TestCtx) -> Outcome {
    let dir = check_out!(links(ctx));
    check_errno!(
        fsutil::open_symlink(&dir.join("target.txt")),
        fsutil::EINVAL,
        "O_SYMLINK on regular file"
    );
    Outcome::Pass
}

fn edge_symlink_o_symlink_on_dir(ctx: &TestCtx) -> Outcome {
    let dir = check_out!(links(ctx));
    check_errno!(
        fsutil::open_symlink(&dir),
        fsutil::EINVAL,
        "O_SYMLINK on directory"
    );
    Outcome::Pass
}

fn edge_symlink_missing_readlink(ctx: &TestCtx) -> Outcome {
    let dir = check_out!(links(ctx));
    check_errno!(
        fsutil::readlink(&dir.join(".__no_such_link__")),
        fsutil::ENOENT,
        "readlink missing"
    );
    Outcome::Pass
}

fn edge_unlink_symlink_keeps_target(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let link = ctx.fixtures().join("deletable/once_symlink");
    match fsutil::is_symlink(&link) {
        Ok(true) => {}
        Ok(false) => {
            return Outcome::Skip("deletable/once_symlink is not a symlink".into());
        }
        Err(e) if fsutil::is_errno(&e, fsutil::ENOENT) => {
            return Outcome::Skip(
                "deletable/once_symlink missing - re-run scripts/gen-fixtures.sh".into(),
            );
        }
        Err(e) => return Outcome::Fail(format!("lstat once_symlink: {e}")),
    }
    let target = ctx.fixtures().join("links/target.txt");
    check_ok!(fsutil::unlink(&link));
    check!(fsutil::gone(&link), "symlink name still opens after unlink");
    check_bytes!(check_ok!(fsutil::read_to_vec(&target)), b"link-target\n");
    Outcome::Pass
}

fn symlink_create(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_link");
    check_ok!(fsutil::symlink("nowhere", &path));
    check_str!(check_ok!(fsutil::readlink(&path)), "nowhere");
    check!(
        check_ok!(fsutil::is_symlink(&path)),
        "guest-created name is not a symlink"
    );
    Outcome::Pass
}

fn symlink_create_follow(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("create_follow_tgt.txt");
    check_ok!(fsutil::write_all_path(&target, b"through-guest-link\n"));
    let name = fsutil::base_name(&target);
    let link = ctx.work("create_follow_link");
    check_ok!(fsutil::symlink(name, &link));
    check_eq!(
        check_ok!(fsutil::read_to_vec(&link)),
        b"through-guest-link\n"
    );
    let f = check_ok!(fsutil::open_ro(&link));
    let p = check_ok!(fsutil::fpath(&f));
    check_eq!(p.rsplit('/').next().unwrap_or(""), name);
    Outcome::Pass
}

fn symlink_create_lstat(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_lstat");
    let target = "./link_test/file.txt";
    check_ok!(fsutil::symlink(target, &path));
    check!(check_ok!(fsutil::is_symlink(&path)), "not a symlink");
    check_eq!(
        check_ok!(fsutil::lstat(&path)).st_size as u64,
        target.len() as u64
    );
    Outcome::Pass
}

fn symlink_create_readdir(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_readdir_link");
    let name = fsutil::base_name(&path).to_owned();
    check_ok!(fsutil::symlink("target.txt", &path));
    check!(
        check_ok!(fsutil::listed(ctx.root(), &name)),
        "guest-created `{name}` missing from parent readdir"
    );
    Outcome::Pass
}

fn symlink_create_unlink_keeps_target(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("create_ul_tgt.txt");
    check_ok!(fsutil::write_all_path(&target, b"keep-me\n"));
    let name = fsutil::base_name(&target);
    let link = ctx.work("create_ul_link");
    check_ok!(fsutil::symlink(name, &link));
    check_ok!(fsutil::unlink(&link));
    check!(fsutil::gone(&link), "link name still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&target)), b"keep-me\n");
    Outcome::Pass
}

fn edge_o_symlink_read_offset(ctx: &TestCtx) -> Outcome {
    let path = check_out!(require_link(ctx, "to_file_dot"));
    let mut f = check_ok!(fsutil::open_symlink(&path));
    check_ok!(f.seek(SeekFrom::Start(2)));
    let mut buf = [0u8; 6];
    let n = check_ok!(f.read(&mut buf));
    check_eq!(&buf[..n], b"target");
    Outcome::Pass
}

fn edge_symlink_create_empty_target(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_empty_tgt");
    match fsutil::symlink("", &path) {
        Err(e) if fsutil::is_errno(&e, fsutil::ENOENT) || fsutil::is_errno(&e, fsutil::EINVAL) => {
            match fsutil::readlink(&path) {
                Err(e2) if fsutil::is_errno(&e2, fsutil::ENOENT) => Outcome::PassNote(format!(
                    "host rejected empty target ({e:?}); driver forwarded FUSE_SYMLINK"
                )),
                other => Outcome::Fail(format!(
                    "symlink() {e:?} but readlink after that is {other:?}"
                )),
            }
        }
        Err(e) => Outcome::Fail(format!("symlink(\"\", name): {e:?}")),
        Ok(()) => {
            check_str!(check_ok!(fsutil::readlink(&path)), "");
            Outcome::Pass
        }
    }
}

fn edge_symlink_create_existing_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_over_file");
    check_ok!(fsutil::write_all_path(&path, b"not-a-link"));
    check_errno!(
        fsutil::symlink("x", &path),
        fsutil::EEXIST,
        "symlink over file"
    );
    check_bytes!(check_ok!(fsutil::read_to_vec(&path)), b"not-a-link");
    Outcome::Pass
}

fn edge_symlink_create_existing_symlink(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_over_link");
    check_ok!(fsutil::symlink("first", &path));
    check_errno!(
        fsutil::symlink("second", &path),
        fsutil::EEXIST,
        "symlink over symlink"
    );
    check_str!(check_ok!(fsutil::readlink(&path)), "first");
    Outcome::Pass
}

fn edge_symlink_create_close_without_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_no_write");
    drop(check_ok!(fsutil::open_creat_symlink(&path)));
    check_errno!(
        fsutil::readlink(&path),
        fsutil::ENOENT,
        "readlink after close without write"
    );
    Outcome::Pass
}

fn edge_symlink_create_write_offset(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_off");
    let mut f = check_ok!(fsutil::open_creat_symlink(&path));
    check_ok!(f.seek(SeekFrom::Start(1)));
    check_errno!(f.write(b"tgt"), fsutil::EINVAL, "write at offset 1");
    Outcome::Pass
}

fn edge_symlink_create_second_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_2write");
    let mut f = check_ok!(fsutil::open_creat_symlink(&path));
    check_ok!(f.write_all(b"first"));
    check_errno!(
        f.write_all(b"second"),
        fsutil::EBADF,
        "second write on pending create"
    );
    drop(f);
    check_str!(check_ok!(fsutil::readlink(&path)), "first");
    Outcome::Pass
}

fn edge_symlink_create_fstat_before_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_fstat");
    let f = check_ok!(fsutil::open_creat_symlink(&path));
    check_errno!(f.metadata(), fsutil::EBADF, "fstat on pending create");
    Outcome::Pass
}

fn edge_symlink_create_o_directory(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_dirflag");
    check_errno!(
        fsutil::open_flags(
            &path,
            flag::O_WRONLY | flag::O_CREAT | flag::O_DIRECTORY | fsutil::O_SYMLINK,
            0o777,
        ),
        fsutil::EINVAL,
        "O_DIRECTORY|O_SYMLINK|O_CREAT"
    );
    Outcome::Pass
}

fn edge_symlink_create_rdonly_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("create_ro_write");
    let mut f = check_ok!(fsutil::open_flags(
        &path,
        flag::O_RDONLY | flag::O_CREAT | fsutil::O_SYMLINK,
        0o777,
    ));
    check_errno!(
        f.write(b"tgt"),
        fsutil::EBADF,
        "write on O_RDONLY pending symlink"
    );
    drop(f);
    check_errno!(
        fsutil::readlink(&path),
        fsutil::ENOENT,
        "read-only pending create must not plant a symlink"
    );
    Outcome::Pass
}
