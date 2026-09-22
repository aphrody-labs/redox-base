use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use libredox::flag;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_eq, check_errno, check_ok,
    check_out, check_str, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func link_file_same_dir,
        func link_file_other_dir,
        func link_unlink_one_keeps_other,
        func link_write_both_names,
        func link_nlink,
        func link_symlink,
        func link_fpath_unchanged,
        func link_then_rename_one,
        func link_same_inode,
        func link_chmod_both,
        func link_truncate_both,
        func link_stat_after,
        func link_cross_dir_readdir,
        func link_open_src_keeps_io,
        func link_o_trunc_one,
        func link_three_names,
        func link_unlink_both_gone,
        func link_then_fsync,
        func link_unlink_src_keeps_dest,
        func link_recreate_unlinked,
        func link_symlink_then_follow,
        func link_then_rename_src,
        func link_nested_dest,
        func link_utimens_both,
        func link_fpath_dest,
        func link_dangling_symlink,
        func link_excl_create_dest,
        func link_creat_opens_inode,
        func link_open_both,
        func link_dest_through_symlink_parent,
        func link_via_linkat,
        edge edge_link_eexist,
        edge edge_link_dir,
        edge edge_link_missing_parent,
        edge edge_link_through_file,
        edge edge_link_spaces,
        edge edge_link_missing_src,
        edge edge_link_same_name,
        edge edge_link_onto_dir,
        edge edge_link_onto_symlink,
        edge edge_link_eexist_keeps_src,
        edge edge_link_name_max,
        edge edge_link_missing_intermediate,
    ]
}

fn linked_pair(
    ctx: &TestCtx,
    src: &str,
    dst: &str,
    payload: &[u8],
) -> Result<(PathBuf, PathBuf), Outcome> {
    let src = ctx.work(src);
    let dst = ctx.work(dst);
    fsutil::write_all_path(&src, payload).map_err(|e| Outcome::Fail(format!("write: {e}")))?;
    fsutil::link(&src, &dst).map_err(|e| Outcome::Fail(format!("link: {e}")))?;
    Ok((src, dst))
}

fn link_file_same_dir(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_src.txt", "ln_dst.txt", b"shared"));
    let src_name = fsutil::base_name(&src).to_owned();
    let dst_name = fsutil::base_name(&dst).to_owned();
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"shared");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"shared");
    check!(
        check_ok!(fsutil::listed(ctx.root(), &src_name)),
        "src missing from readdir"
    );
    check!(
        check_ok!(fsutil::listed(ctx.root(), &dst_name)),
        "dest missing from readdir"
    );
    Outcome::Pass
}

fn link_file_other_dir(ctx: &TestCtx) -> Outcome {
    let dir = ctx.work("ln_dir");
    check_ok!(fsutil::make_dir(&dir));
    let src = ctx.work("ln_into.txt");
    let dst = dir.join("inside.txt");
    check_ok!(fsutil::write_all_path(&src, b"cross"));
    check_ok!(fsutil::link(&src, &dst));
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"cross");
    check!(!fsutil::gone(&src), "src disappeared");
    Outcome::Pass
}

fn link_unlink_one_keeps_other(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_keep.txt", "ln_drop.txt", b"stay"));
    check_ok!(fsutil::unlink(&dst));
    check!(fsutil::gone(&dst), "dest still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"stay");
    Outcome::Pass
}

fn link_write_both_names(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_wr_a.txt", "ln_wr_b.txt", b"v1"));
    let mut f = check_ok!(fsutil::open_rw(&dst));
    check_ok!(f.seek(SeekFrom::Start(0)));
    check_ok!(f.write_all(b"v2"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"v2");
    Outcome::Pass
}

fn link_nlink(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_nlink.txt");
    let dst = ctx.work("ln_nlink2.txt");
    check_ok!(fsutil::write_all_path(&src, b"n"));
    check_eq!(check_ok!(fsutil::stat(&src)).st_nlink as u64, 1);
    check_ok!(fsutil::link(&src, &dst));
    check_eq!(check_ok!(fsutil::stat(&src)).st_nlink as u64, 2);
    check_eq!(check_ok!(fsutil::stat(&dst)).st_nlink as u64, 2);
    check_ok!(fsutil::unlink(&dst));
    check_eq!(check_ok!(fsutil::stat(&src)).st_nlink as u64, 1);
    Outcome::Pass
}

fn link_symlink(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("ln_sym_tgt.txt");
    let src = ctx.work("ln_sym");
    let dst = ctx.work("ln_sym2");
    check_ok!(fsutil::write_all_path(&target, b"via"));
    let target_name = fsutil::base_name(&target);
    check_ok!(fsutil::symlink(target_name, &src));
    check_ok!(fsutil::link(&src, &dst));
    check!(check_ok!(fsutil::is_symlink(&src)), "src not a symlink");
    check!(check_ok!(fsutil::is_symlink(&dst)), "dest not a symlink");
    check_str!(check_ok!(fsutil::readlink(&src)), target_name);
    check_str!(check_ok!(fsutil::readlink(&dst)), target_name);
    Outcome::Pass
}

fn link_fpath_unchanged(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_fpath.txt");
    let dst = ctx.work("ln_fpath2.txt");
    let src_name = fsutil::base_name(&src).to_owned();
    check_ok!(fsutil::write_all_path(&src, b"p"));
    let f = check_ok!(fsutil::open_stat(&src));
    check_ok!(fsutil::link(&src, &dst));
    let p = check_ok!(fsutil::fpath(&f));
    check!(p.contains(&src_name), "source fpath changed: {p}");
    check!(!p.contains("ln_fpath2"), "fpath became dest: {p}");
    Outcome::Pass
}

fn link_then_rename_one(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_rn_a.txt", "ln_rn_b.txt", b"both"));
    let moved = ctx.work("ln_rn_moved.txt");
    check_ok!(fsutil::rename(&dst, &moved));
    check!(!fsutil::gone(&src), "src disappeared");
    check!(fsutil::gone(&dst), "old dest still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"both");
    check_bytes!(check_ok!(fsutil::read_to_vec(&moved)), b"both");
    Outcome::Pass
}

fn edge_link_eexist(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_e_src.txt");
    let dst = ctx.work("ln_e_dst.txt");
    check_ok!(fsutil::write_all_path(&src, b"a"));
    check_ok!(fsutil::write_all_path(&dst, b"b"));
    check_errno!(
        fsutil::link(&src, &dst),
        fsutil::EEXIST,
        "link onto existing"
    );
    Outcome::Pass
}

fn edge_link_dir(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_e_dir");
    let dst = ctx.work("ln_e_dir2");
    check_ok!(fsutil::make_dir(&src));
    check_errno!(fsutil::link(&src, &dst), fsutil::EPERM, "link dir");
    Outcome::Pass
}

fn edge_link_missing_parent(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_e_orphan.txt");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    check_errno!(
        fsutil::link(&src, &ctx.work("ln_no_such_dir").join("x.txt")),
        fsutil::ENOENT,
        "missing dest parent"
    );
    Outcome::Pass
}

fn edge_link_through_file(ctx: &TestCtx) -> Outcome {
    let file = ctx.work("ln_e_notdir.txt");
    let src = ctx.work("ln_e_through.txt");
    check_ok!(fsutil::write_all_path(&file, b"x"));
    check_ok!(fsutil::write_all_path(&src, b"y"));
    check_errno!(
        fsutil::link(&src, &file.join("child")),
        fsutil::ENOTDIR,
        "through file"
    );
    Outcome::Pass
}

fn edge_link_spaces(ctx: &TestCtx) -> Outcome {
    let (_, dst) = check_out!(linked_pair(
        ctx,
        "ln src spaces.txt",
        "ln dst spaces.txt",
        b"spaces-ok"
    ));
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"spaces-ok");
    Outcome::Pass
}

fn edge_link_missing_src(ctx: &TestCtx) -> Outcome {
    check_errno!(
        fsutil::link(
            &ctx.work("ln_e_missing.txt"),
            &ctx.work("ln_e_missing_dst.txt")
        ),
        fsutil::ENOENT,
        "missing src"
    );
    Outcome::Pass
}

fn link_same_inode(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_ino_a.txt", "ln_ino_b.txt", b"same"));
    check_eq!(
        check_ok!(fsutil::stat(&src)).st_ino,
        check_ok!(fsutil::stat(&dst)).st_ino
    );
    Outcome::Pass
}

fn link_chmod_both(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_chmod_a.txt", "ln_chmod_b.txt", b"m"));
    check_ok!(fsutil::chmod(&dst, 0o640));
    check_eq!(check_ok!(fsutil::mode_of(&src)), 0o640);
    check_eq!(check_ok!(fsutil::mode_of(&dst)), 0o640);
    Outcome::Pass
}

fn link_truncate_both(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(
        ctx,
        "ln_trunc_a.txt",
        "ln_trunc_b.txt",
        b"0123456789"
    ));
    let f = check_ok!(fsutil::open_rw(&dst));
    check_ok!(f.set_len(4));
    drop(f);
    check_eq!(check_ok!(fsutil::stat(&src)).st_size as u64, 4);
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"0123");
    Outcome::Pass
}

fn link_stat_after(ctx: &TestCtx) -> Outcome {
    let payload = b"stat-me";
    let (_, dst) = check_out!(linked_pair(ctx, "ln_stat_a.txt", "ln_stat_b.txt", payload));
    check_eq!(
        check_ok!(fsutil::stat(&dst)).st_size as u64,
        payload.len() as u64
    );
    check_eq!(check_ok!(fsutil::type_of(&dst)), fsutil::MODE_FILE);
    Outcome::Pass
}

fn link_cross_dir_readdir(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("ln_rd_a");
    let b = ctx.work("ln_rd_b");
    check_ok!(fsutil::make_dir(&a));
    check_ok!(fsutil::make_dir(&b));
    let src = a.join("shared.txt");
    let dst = b.join("shared.txt");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    check_ok!(fsutil::link(&src, &dst));
    check!(
        check_ok!(fsutil::listed(&a, "shared.txt")),
        "missing in source dir"
    );
    check!(
        check_ok!(fsutil::listed(&b, "shared.txt")),
        "missing in dest dir"
    );
    Outcome::Pass
}

fn link_open_src_keeps_io(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_open.txt");
    let dst = ctx.work("ln_open2.txt");
    let mut f = check_ok!(fsutil::create_rw(&src));
    check_ok!(f.write_all(b"still-here"));
    check_ok!(fsutil::link(&src, &dst));
    check_ok!(f.seek(SeekFrom::Start(0)));
    let mut buf = Vec::new();
    check_ok!(f.read_to_end(&mut buf));
    check_eq!(buf.as_slice(), b"still-here");
    Outcome::Pass
}

fn link_o_trunc_one(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(
        ctx,
        "ln_otrunc_a.txt",
        "ln_otrunc_b.txt",
        b"old-payload"
    ));
    let mut f = check_ok!(fsutil::open_flags(&dst, flag::O_RDWR | flag::O_TRUNC, 0));
    check_ok!(f.write_all(b"new"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"new");
    Outcome::Pass
}

fn link_three_names(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("ln_3_a.txt");
    let b = ctx.work("ln_3_b.txt");
    let c = ctx.work("ln_3_c.txt");
    check_ok!(fsutil::write_all_path(&a, b"tri"));
    check_ok!(fsutil::link(&a, &b));
    check_ok!(fsutil::link(&b, &c));
    check_eq!(check_ok!(fsutil::stat(&a)).st_nlink as u64, 3);
    check_ok!(fsutil::unlink(&b));
    check_eq!(check_ok!(fsutil::stat(&c)).st_nlink as u64, 2);
    check_bytes!(check_ok!(fsutil::read_to_vec(&c)), b"tri");
    Outcome::Pass
}

fn link_unlink_both_gone(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_both.txt", "ln_both2.txt", b"gone"));
    check_ok!(fsutil::unlink(&src));
    check_ok!(fsutil::unlink(&dst));
    check!(fsutil::gone(&src), "src still opens");
    check!(fsutil::gone(&dst), "dest still opens");
    Outcome::Pass
}

fn link_then_fsync(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_sync.txt", "ln_sync2.txt", b"sync-me"));
    let f = check_ok!(fsutil::open_rw(&dst));
    check_ok!(fsutil::fsync(&f));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"sync-me");
    Outcome::Pass
}

fn edge_link_same_name(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("ln_e_same.txt");
    check_ok!(fsutil::write_all_path(&path, b"x"));
    check_errno!(fsutil::link(&path, &path), fsutil::EEXIST, "link to self");
    Outcome::Pass
}

fn edge_link_onto_dir(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_e_file.txt");
    let dst = ctx.work("ln_e_onto_dir");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    check_ok!(fsutil::make_dir(&dst));
    check_errno!(fsutil::link(&src, &dst), fsutil::EEXIST, "link onto dir");
    Outcome::Pass
}

fn edge_link_onto_symlink(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("ln_e_ontgt.txt");
    let dst = ctx.work("ln_e_onsym");
    let src = ctx.work("ln_e_onsrc.txt");
    check_ok!(fsutil::write_all_path(&target, b"t"));
    check_ok!(fsutil::symlink(fsutil::base_name(&target), &dst));
    check_ok!(fsutil::write_all_path(&src, b"s"));
    check_errno!(
        fsutil::link(&src, &dst),
        fsutil::EEXIST,
        "link onto symlink"
    );
    check!(check_ok!(fsutil::is_symlink(&dst)), "dest symlink replaced");
    Outcome::Pass
}

fn link_unlink_src_keeps_dest(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_usrc.txt", "ln_udst.txt", b"keep-dest"));
    check_ok!(fsutil::unlink(&src));
    check!(fsutil::gone(&src), "src still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"keep-dest");
    check_eq!(check_ok!(fsutil::stat(&dst)).st_nlink as u64, 1);
    Outcome::Pass
}

fn link_recreate_unlinked(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_re_a.txt", "ln_re_b.txt", b"shared"));
    check_ok!(fsutil::unlink(&dst));
    check_ok!(fsutil::write_all_path(&dst, b"new"));
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"shared");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"new");
    check!(
        check_ok!(fsutil::stat(&src)).st_ino != check_ok!(fsutil::stat(&dst)).st_ino,
        "recreated dest reused inode"
    );
    Outcome::Pass
}

fn link_symlink_then_follow(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("ln_fol_tgt.txt");
    let src = ctx.work("ln_fol_link");
    let dst = ctx.work("ln_fol_link2");
    check_ok!(fsutil::write_all_path(&target, b"followed"));
    let target_name = fsutil::base_name(&target);
    check_ok!(fsutil::symlink(target_name, &src));
    check_ok!(fsutil::link(&src, &dst));
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"followed");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"followed");
    check_str!(check_ok!(fsutil::readlink(&src)), target_name);
    check_str!(check_ok!(fsutil::readlink(&dst)), target_name);
    Outcome::Pass
}

fn link_then_rename_src(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_rnsrc.txt", "ln_rndst.txt", b"both"));
    let moved = ctx.work("ln_rnmoved.txt");
    check_ok!(fsutil::rename(&src, &moved));
    check!(fsutil::gone(&src), "old src still opens");
    check_bytes!(check_ok!(fsutil::read_to_vec(&moved)), b"both");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"both");
    check_eq!(
        check_ok!(fsutil::stat(&moved)).st_ino,
        check_ok!(fsutil::stat(&dst)).st_ino
    );
    Outcome::Pass
}

fn link_nested_dest(ctx: &TestCtx) -> Outcome {
    let a = ctx.work("ln_nest_a");
    check_ok!(fsutil::make_dir(&a));
    let b = a.join("b");
    check_ok!(fsutil::make_dir(&b));
    let src = ctx.work("ln_nest_src.txt");
    let dst = b.join("leaf.txt");
    check_ok!(fsutil::write_all_path(&src, b"deep"));
    check_ok!(fsutil::link(&src, &dst));
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"deep");
    check!(
        check_ok!(fsutil::listed(&b, "leaf.txt")),
        "missing in nested dest dir"
    );
    Outcome::Pass
}

fn link_utimens_both(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(ctx, "ln_time_a.txt", "ln_time_b.txt", b"t"));
    let f = check_ok!(fsutil::open_rw(&dst));
    let want = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    check_ok!(fsutil::set_times_secs(&f, 1_700_000_000));
    drop(f);
    let got = check_ok!(fsutil::mtime(&src));
    check!(
        fsutil::close_enough(got, want, 1),
        "src mtime {got:?} not within 1s of {want:?}"
    );
    Outcome::Pass
}

fn link_fpath_dest(ctx: &TestCtx) -> Outcome {
    let (_, dst) = check_out!(linked_pair(ctx, "ln_fpd_src.txt", "ln_fpd_dst.txt", b"p"));
    let dst_name = fsutil::base_name(&dst).to_owned();
    let f = check_ok!(fsutil::open_stat(&dst));
    let p = check_ok!(fsutil::fpath(&f));
    check!(p.contains(&dst_name), "dest fpath missing dest name: {p}");
    check!(!p.contains("ln_fpd_src"), "dest fpath is src: {p}");
    Outcome::Pass
}

fn link_dangling_symlink(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_dang");
    let dst = ctx.work("ln_dang2");
    check_ok!(fsutil::symlink("no-such-target", &src));
    check_ok!(fsutil::link(&src, &dst));
    check!(check_ok!(fsutil::is_symlink(&src)), "src not a symlink");
    check!(check_ok!(fsutil::is_symlink(&dst)), "dest not a symlink");
    check_str!(check_ok!(fsutil::readlink(&src)), "no-such-target");
    check_str!(check_ok!(fsutil::readlink(&dst)), "no-such-target");
    check_errno!(fsutil::open_ro(&src), fsutil::ENOENT, "follow src");
    check_errno!(fsutil::open_ro(&dst), fsutil::ENOENT, "follow dest");
    Outcome::Pass
}

fn link_excl_create_dest(ctx: &TestCtx) -> Outcome {
    let (_, dst) = check_out!(linked_pair(ctx, "ln_excl_a.txt", "ln_excl_b.txt", b"taken"));
    check_errno!(fsutil::create_excl(&dst), fsutil::EEXIST, "O_EXCL dest");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"taken");
    Outcome::Pass
}

fn link_creat_opens_inode(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(
        ctx,
        "ln_creat_a.txt",
        "ln_creat_b.txt",
        b"orig-payload"
    ));
    let mut f = check_ok!(fsutil::open_flags(
        &dst,
        flag::O_RDWR | flag::O_CREAT,
        0o644
    ));
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"orig-payload");
    check_ok!(f.set_len(0));
    check_ok!(f.write_all(b"via-creat"));
    drop(f);
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"via-creat");
    Outcome::Pass
}

fn link_open_both(ctx: &TestCtx) -> Outcome {
    let (src, dst) = check_out!(linked_pair(
        ctx,
        "ln_bothfd_a.txt",
        "ln_bothfd_b.txt",
        b"........"
    ));
    let mut a = check_ok!(fsutil::open_rw(&src));
    let mut b = check_ok!(fsutil::open_rw(&dst));
    check_ok!(fsutil::write_at(&mut a, 0, b"hello"));
    check_bytes!(check_ok!(fsutil::read_at(&mut b, 0, 5)), b"hello");
    Outcome::Pass
}

fn link_dest_through_symlink_parent(ctx: &TestCtx) -> Outcome {
    let real = ctx.work("ln_realdir");
    let linkdir = ctx.work("ln_linkdir");
    check_ok!(fsutil::make_dir(&real));
    check_ok!(fsutil::symlink(fsutil::base_name(&real), &linkdir));
    let src = ctx.work("ln_thr_src.txt");
    let dst = linkdir.join("child.txt");
    check_ok!(fsutil::write_all_path(&src, b"via-parent-link"));
    check_ok!(fsutil::link(&src, &dst));
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&real.join("child.txt"))),
        b"via-parent-link"
    );
    Outcome::Pass
}

fn link_via_linkat(ctx: &TestCtx) -> Outcome {
    let dir = ctx.work("ln_at_dir");
    check_ok!(fsutil::make_dir(&dir));
    let src = ctx.work("ln_at_src.txt");
    check_ok!(fsutil::write_all_path(&src, b"at"));
    let d = check_ok!(fsutil::open_dir(&dir));
    check_ok!(fsutil::linkat_into(&src, &d, "inside.txt"));
    drop(d);
    check_bytes!(
        check_ok!(fsutil::read_to_vec(&dir.join("inside.txt"))),
        b"at"
    );
    Outcome::Pass
}

fn edge_link_eexist_keeps_src(ctx: &TestCtx) -> Outcome {
    let src = ctx.work("ln_e_keep_src.txt");
    let dst = ctx.work("ln_e_keep_dst.txt");
    check_ok!(fsutil::write_all_path(&src, b"keep"));
    check_ok!(fsutil::write_all_path(&dst, b"other"));
    check_errno!(
        fsutil::link(&src, &dst),
        fsutil::EEXIST,
        "link onto existing"
    );
    check_bytes!(check_ok!(fsutil::read_to_vec(&src)), b"keep");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dst)), b"other");
    Outcome::Pass
}

fn edge_link_name_max(ctx: &TestCtx) -> Outcome {
    let dirp = ctx.work("ln_namemax");
    check_ok!(fsutil::make_dir(&dirp));
    let src = dirp.join("src.txt");
    check_ok!(fsutil::write_all_path(&src, b"n"));
    let n255 = "n".repeat(255);
    let n256 = "n".repeat(256);
    check_ok!(fsutil::link(&src, &dirp.join(&n255)));
    check_bytes!(check_ok!(fsutil::read_to_vec(&dirp.join(&n255))), b"n");
    check_errno!(
        fsutil::link(&src, &dirp.join(&n256)),
        fsutil::ENAMETOOLONG,
        "link 256"
    );
    Outcome::Pass
}

fn edge_link_missing_intermediate(ctx: &TestCtx) -> Outcome {
    let mid = ctx.work("ln_e_mid");
    check_ok!(fsutil::make_dir(&mid));
    let src = ctx.work("ln_e_mid_src.txt");
    check_ok!(fsutil::write_all_path(&src, b"x"));
    check_errno!(
        fsutil::link(&src, &mid.join("no_such").join("x.txt")),
        fsutil::ENOENT,
        "missing intermediate"
    );
    Outcome::Pass
}
