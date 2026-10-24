use std::io::{Read, Seek, SeekFrom, Write};

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func rename_file,
        func rename_dir,
        func rename_overwrite,
        func rename_symlink,
        func rename_open_keeps_io,
        func rename_attr_follows,
        func rename_cross_dir,
        func rename_at_same_dir,
        func rename_at_cross_dir,
        func rename_at_overwrite,
        edge edge_rename_errnos,
        edge edge_rename_exdev,
        edge edge_rename_at_noreplace,
        edge edge_rename_at,
    ]
}

fn rename_file(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_src.txt");
    let dst = ctx.work("rn_dst.txt");
    let src_name = fsutil::base_name(&src).to_owned();
    let dst_name = fsutil::base_name(&dst).to_owned();
    check_ok!(fsutil::write_all_path(&src, b"moved"));
    check_ok!(fsutil::rename(&src, &dst));
    check!(fsutil::gone(&src), "old name still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"moved");
    check!(
        !check_ok!(fsutil::listed(ctx.root(), &src_name)),
        "old name still in readdir"
    );
    check!(
        check_ok!(fsutil::listed(ctx.root(), &dst_name)),
        "new name missing from readdir"
    );

    check_ok!(fsutil::rename(&dst, &dst));
    check_ok!(fsutil::write_all_path(&src, b"new"));
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"new");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"moved");
    check_ok!(fsutil::unlink(&dst));
    check!(fsutil::gone(&dst), "dest still opens");
    check!(!fsutil::gone(&src), "recreated old disappeared");

    let spaced_src = ctx.work("rn src spaces.txt");
    let spaced_dst = ctx.work("rn dst spaces.txt");
    check_ok!(fsutil::write_all_path(&spaced_src, b"spaces-ok"));
    check_ok!(fsutil::rename(&spaced_src, &spaced_dst));
    check!(fsutil::gone(&spaced_src), "spaced old still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&spaced_dst)), b"spaces-ok");
    Outcome::Pass
}

fn rename_dir(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_srcdir");
    let dst = ctx.work("rn_dstdir");
    let dst_name = fsutil::base_name(&dst).to_owned();
    check_ok!(fsutil::make_dir(&src));
    let leaf = src.join("leaf.txt");
    check_ok!(fsutil::write_all_path(&leaf, b"kid"));
    let mut child = check_ok!(fsutil::open_rw(&leaf));
    check_ok!(fsutil::rename(&src, &dst));
    check!(fsutil::gone(&src), "old dir still opens");
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&dst.join("leaf.txt"))),
        b"kid"
    );
    let p = check_ok!(fsutil::fpath(&child));
    check!(
        p.contains(&dst_name) && p.contains("leaf.txt"),
        "child fpath {p}"
    );
    check_ok!(child.seek(SeekFrom::Start(0)));
    let mut buf = Vec::new();
    check_ok!(child.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"kid");

    let empty = ctx.work("rn_ow_empty");
    check_ok!(fsutil::make_dir(&empty));
    check_ok!(fsutil::rename(&dst, &empty));
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&empty.join("leaf.txt"))),
        b"kid"
    );
    check_ok!(fsutil::unlink(&empty.join("leaf.txt")));
    check_ok!(fsutil::rmdir(&empty));
    check!(fsutil::gone(&empty), "dest still opens");
    check!(fsutil::gone(&src), "old dir reappeared");
    Outcome::Pass
}

fn rename_overwrite(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_ow_src.txt");
    let dst = ctx.work("rn_ow_dst.txt");
    check_ok!(fsutil::write_all_path(&src, b"winner"));
    check_ok!(fsutil::write_all_path(&dst, b"loser"));
    let mut old = check_ok!(fsutil::open_rw(&dst));
    check_ok!(fsutil::rename(&src, &dst));
    check!(fsutil::gone(&src), "old name still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"winner");
    check_ok!(old.seek(SeekFrom::Start(0)));
    let mut buf = Vec::new();
    check_ok!(old.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"loser");

    let mut trunc = check_ok!(fsutil::open_flags(&dst, flag::O_RDWR | flag::O_TRUNC, 0));
    check_ok!(trunc.write_all(b"new"));
    drop(trunc);
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"new");

    let f = check_ok!(fsutil::create_excl(&src));
    drop(f);
    Outcome::Pass
}

fn rename_symlink(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("rn_link_tgt.txt");
    let src = ctx.work("rn_link");
    let dst = ctx.work("rn_link_moved");
    check_ok!(fsutil::write_all_path(&target, b"via-link"));
    let target_name = fsutil::base_name(&target).to_owned();
    check_ok!(fsutil::symlink(&target_name, &src));
    check_ok!(fsutil::rename(&src, &dst));
    check!(fsutil::gone(&src), "old symlink still there");
    check!(check_ok!(fsutil::is_symlink(&dst)), "dest is not a symlink");
    let got_link = check_ok!(fsutil::readlink(&dst));
    check_eq!(got_link.as_str(), target_name.as_str());
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"via-link");

    let file = ctx.work("rn_onsym_src.txt");
    check_ok!(fsutil::write_all_path(&file, b"file-wins"));
    check_ok!(fsutil::rename(&file, &dst));
    check!(!check_ok!(fsutil::is_symlink(&dst)), "dest still a symlink");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"file-wins");
    check_bytes!(check_ok!(fsutil::read_to_vec(&target)), b"via-link");
    Outcome::Pass
}

fn rename_open_keeps_io(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_open.txt");
    let dst = ctx.work("rn_open_moved.txt");
    let dst_name = fsutil::base_name(&dst).to_owned();
    let mut f = check_ok!(fsutil::create_rw(&src));
    check_ok!(f.write_all(b"still-here"));
    let st = check_ok!(fsutil::open_stat(&src));
    check_ok!(fsutil::rename(&src, &dst));
    check_ok!(f.seek(SeekFrom::Start(0)));
    let mut buf = Vec::new();
    check_ok!(f.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"still-here");
    let p = check_ok!(fsutil::fpath(&st));
    check!(p.contains(&dst_name), "fpath still old path: {p}");
    Outcome::Pass
}

fn rename_attr_follows(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_attr.txt");
    let dst = ctx.work("rn_attr_moved.txt");
    check_ok!(fsutil::write_all_path(&src, b"0123456789"));
    check_ok!(fsutil::chmod(&src, 0o640));
    let f = check_ok!(fsutil::open_rw(&src));
    check_ok!(f.set_len(4));
    drop(f);
    check_ok!(fsutil::rename(&src, &dst));
    check_eq!(check_ok!(fsutil::mode_of(&dst)), 0o640);
    check_eq!(check_ok!(fsutil::stat(&dst)).st_size as u64, 4);
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"0123");
    check_eq!(check_ok!(fsutil::type_of(&dst)), fsutil::MODE_FILE);
    check_ok!(fsutil::chmod(&dst, 0o600));
    check_eq!(check_ok!(fsutil::mode_of(&dst)), 0o600);
    let f = check_ok!(fsutil::open_rw(&dst));
    check_ok!(fsutil::fsync(&f));
    Outcome::Pass
}

fn rename_cross_dir(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("rn_rd_a");
    let b = ctx.work("rn_rd_b");
    check_ok!(fsutil::make_dir(&a));
    check_ok!(fsutil::make_dir(&b));
    let src = a.join("moved.txt");
    let dst = b.join("moved.txt");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    check_ok!(fsutil::rename(&src, &dst));
    check!(
        !check_ok!(fsutil::listed(&a, "moved.txt")),
        "still in source"
    );
    check!(
        check_ok!(fsutil::listed(&b, "moved.txt")),
        "missing in dest"
    );
    Outcome::Pass
}

fn edge_rename_errnos(ctx: &TestCtx) -> Outcome {
    let file = ctx.work("rn_e_file.txt");
    let dir = ctx.work("rn_e_dir");
    check_ok!(fsutil::write_all_path(&file, b"x"));
    check_ok!(fsutil::make_dir(&dir));
    check_errno!(fsutil::rename(&file, &dir), fsutil::EISDIR, "file onto dir");
    check_errno!(
        fsutil::rename(&dir, &file),
        fsutil::ENOTDIR,
        "dir onto file"
    );

    let full = ctx.work("rn_e_full");
    let empty = ctx.work("rn_e_empty");
    check_ok!(fsutil::make_dir(&full));
    check_ok!(fsutil::make_dir(&empty));
    check_ok!(fsutil::write_all_path(&full.join("keep.txt"), b"k"));
    check_errno!(
        fsutil::rename(&empty, &full),
        fsutil::ENOTEMPTY,
        "onto nonempty dir"
    );

    check_errno!(
        fsutil::rename(
            &ctx.work("rn_e_missing.txt"),
            &ctx.work("rn_e_missing_dst.txt")
        ),
        fsutil::ENOENT,
        "missing src"
    );
    let orphan = ctx.work("rn_e_orphan.txt");
    check_ok!(fsutil::write_all_path(&orphan, b"x"));
    check_errno!(
        fsutil::rename(&orphan, &ctx.work("rn_no_such_dir").join("x.txt")),
        fsutil::ENOENT,
        "missing dest parent"
    );

    let through = ctx.work("rn_e_notdir.txt");
    let src = ctx.work("rn_e_through.txt");
    check_ok!(fsutil::write_all_path(&through, b"x"));
    check_ok!(fsutil::write_all_path(&src, b"y"));
    check_errno!(
        fsutil::rename(&src, &through.join("child")),
        fsutil::ENOTDIR,
        "through file"
    );

    let anc = ctx.work("rn_e_anc");
    check_ok!(fsutil::make_dir(&anc));
    let kid = anc.join("kid");
    check_ok!(fsutil::make_dir(&kid));
    check_errno!(
        fsutil::rename(&anc, &kid.join("oops")),
        fsutil::EINVAL,
        "into descendant"
    );
    check_ok!(fsutil::open_dir(&anc));
    Outcome::Pass
}

fn edge_rename_exdev(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("rn_exdev.txt");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    let name = format!("st-exdev-{}", ctx.cfg.run_id);
    let dests = [
        std::path::PathBuf::from("/tmp").join(&name),
        std::path::PathBuf::from("/scheme/tmp").join(&name),
        std::path::PathBuf::from("/scheme/file").join(&name),
        std::path::PathBuf::from("/scheme/ramfs").join(&name),
    ];
    let mut last_enoent = None;
    for dest in &dests {
        match fsutil::rename(&src, dest) {
            Err(e) if fsutil::is_errno(&e, fsutil::EXDEV) => return Outcome::Pass,
            Err(e) if fsutil::is_errno(&e, fsutil::ENOENT) => {
                last_enoent = Some(e);
                continue;
            }
            Err(e) => {
                return Outcome::Fail(format!("expected EXDEV to {}, got {e:?}", dest.display()));
            }
            Ok(()) => {
                let _ = fsutil::unlink(dest);
                return Outcome::PassNote(format!(
                    "{} is the same filesystem as --root",
                    dest.display()
                ));
            }
        }
    }
    Outcome::Skip(format!(
        "no other scheme to rename onto ({})",
        last_enoent
            .map(|e| e.to_string())
            .unwrap_or_else(|| "none tried".into())
    ))
}

fn rename_at_same_dir(ctx: &TestCtx) -> Outcome {
    let src_name = fsutil::base_name(&ctx.work("at_rn_src.txt")).to_owned();
    let dst_name = fsutil::base_name(&ctx.work("at_rn_dst.txt")).to_owned();
    let src = ctx.root().join(&src_name);
    check_ok!(fsutil::write_all_path(&src, b"moved"));
    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    check_ok!(fsutil::frenameat(&dir, &src_name, &dir, &dst_name, 0));

    let dst = ctx.root().join(&dst_name);
    check!(fsutil::gone(&src), "old name still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"moved");
    check!(
        check_ok!(fsutil::listed(ctx.root(), &dst_name)),
        "new name missing from readdir"
    );
    Outcome::Pass
}

fn rename_at_cross_dir(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("at_rn_a");
    let b = ctx.work("at_rn_b");
    check_ok!(fsutil::make_dir(&a));
    check_ok!(fsutil::make_dir(&b));
    check_ok!(fsutil::write_all_path(&a.join("moved.txt"), b"x"));
    let dir_a = check_ok!(fsutil::open_dir(&a));
    let dir_b = check_ok!(fsutil::open_dir(&b));
    check_ok!(fsutil::frenameat(
        &dir_a,
        "moved.txt",
        &dir_b,
        "moved.txt",
        0
    ));
    check!(
        !check_ok!(fsutil::listed(&a, "moved.txt")),
        "still in source"
    );
    check!(
        check_ok!(fsutil::listed(&b, "moved.txt")),
        "missing in dest"
    );
    check_bytes!(check_ok!(fsutil::read_to_vec(&b.join("moved.txt"))), b"x");
    Outcome::Pass
}

fn rename_at_overwrite(ctx: &TestCtx) -> Outcome {
    let src_name = fsutil::base_name(&ctx.work("at_rn_ow_src.txt")).to_owned();
    let dst_name = fsutil::base_name(&ctx.work("at_rn_ow_dst.txt")).to_owned();
    let src = ctx.root().join(&src_name);
    let dst = ctx.root().join(&dst_name);
    check_ok!(fsutil::write_all_path(&src, b"winner"));
    check_ok!(fsutil::write_all_path(&dst, b"loser"));
    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    check_ok!(fsutil::frenameat(&dir, &src_name, &dir, &dst_name, 0));
    check!(fsutil::gone(&src), "old name still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"winner");
    Outcome::Pass
}

fn edge_rename_at_noreplace(ctx: &TestCtx) -> Outcome {
    let src_name = fsutil::base_name(&ctx.work("at_rn_nr_src.txt")).to_owned();
    let dst_name = fsutil::base_name(&ctx.work("at_rn_nr_dst.txt")).to_owned();
    let fresh_name = fsutil::base_name(&ctx.work("at_rn_nr_fresh.txt")).to_owned();
    let src = ctx.root().join(&src_name);
    let dst = ctx.root().join(&dst_name);
    check_ok!(fsutil::write_all_path(&src, b"keep"));
    check_ok!(fsutil::write_all_path(&dst, b"stay"));
    let dir = check_ok!(fsutil::open_dir(ctx.root()));
    check_errno!(
        fsutil::frenameat(&dir, &src_name, &dir, &dst_name, fsutil::RENAME_NOREPLACE),
        fsutil::EEXIST,
        "noreplace onto existing"
    );
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"keep");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"stay");

    check_ok!(fsutil::frenameat(
        &dir,
        &src_name,
        &dir,
        &fresh_name,
        fsutil::RENAME_NOREPLACE
    ));
    check!(fsutil::gone(&src), "old name still opens");
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&ctx.root().join(&fresh_name))),
        b"keep"
    );
    Outcome::Pass
}

fn edge_rename_at(ctx: &TestCtx) -> Outcome {
    let sub_path = ctx.work("at_rn_edge");
    check_ok!(fsutil::make_dir(&sub_path));
    check_ok!(fsutil::write_all_path(&sub_path.join("old.txt"), b"abs"));
    let sub = check_ok!(fsutil::open_dir(&sub_path));

    check_errno!(
        fsutil::frenameat(&sub, "", &sub, "x.txt", 0),
        fsutil::EINVAL,
        "empty old name"
    );
    check_errno!(
        fsutil::frenameat(&sub, "old.txt", &sub, "", 0),
        fsutil::EINVAL,
        "empty new name"
    );
    check!(
        check_ok!(fsutil::listed(&sub_path, "old.txt")),
        "empty rename removed the source"
    );

    let lru = check_ok!(fsutil::open_ro(&ctx.cfg.debug_root.join("lru_evicted")));
    check_errno!(
        fsutil::frenameat(&lru, "lru_evicted", &lru, "other", 0),
        fsutil::EPERM,
        "debug handle"
    );
    check_ok!(fsutil::open_ro(&ctx.cfg.debug_root.join("lru_evicted")));

    let dst_name = fsutil::base_name(&ctx.work("at_rn_abs.txt")).to_owned();
    let abs_path = ctx.root().join(&dst_name);
    let abs = check_ok!(fsutil::path_to_str(&abs_path)).to_owned();
    check_ok!(fsutil::frenameat(&sub, "old.txt", &sub, &abs, 0));
    check!(
        !check_ok!(fsutil::listed(&sub_path, "old.txt")),
        "old name still in the directory"
    );
    check!(
        !check_ok!(fsutil::listed(&sub_path, &dst_name)),
        "absolute rename landed in the directory fd"
    );
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&ctx.root().join(&dst_name))),
        b"abs"
    );

    check_ok!(fsutil::write_all_path(&sub_path.join("stay.txt"), b"stay"));
    check_errno!(
        fsutil::frenameat(&sub, "stay.txt", &sub, "/scheme/not-virtio-fs/x", 0),
        fsutil::EXDEV,
        "other scheme"
    );
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&sub_path.join("stay.txt"))),
        b"stay"
    );
    Outcome::Pass
}
