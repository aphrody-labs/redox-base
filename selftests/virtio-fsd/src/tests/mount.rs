use crate::{Outcome, TestCase, TestCtx, cases, check, check_ok, fsutil};

pub fn cases() -> Vec<TestCase> {
    cases![
        func mount_root_open_readdir,
        func mount_root_fpath,
        func mount_root_fstat,
        func mount_root_fstatvfs,
    ]
}

fn mount_root_open_readdir(ctx: &TestCtx) -> Outcome {
    check!(fsutil::is_dir(ctx.root()), "root is not a directory");
    check_ok!(fsutil::read_dir_names(ctx.root()));
    Outcome::Pass
}

fn mount_root_fpath(ctx: &TestCtx) -> Outcome {
    let f = check_ok!(fsutil::open_dir(ctx.root()));
    let path = check_ok!(fsutil::fpath(&f));
    let root = ctx.root().to_string_lossy();
    check!(
        path.trim_end_matches('/') == root.trim_end_matches('/') || path.starts_with(root.as_ref()),
        "fpath root `{path}`, expected `{root}`"
    );
    Outcome::Pass
}

fn mount_root_fstat(ctx: &TestCtx) -> Outcome {
    let meta = check_ok!(std::fs::metadata(ctx.root()));
    check!(meta.is_dir(), "root metadata is not a directory");
    let f = check_ok!(fsutil::open_dir(ctx.root()));
    let fd_meta = check_ok!(f.metadata());
    check!(fd_meta.is_dir(), "fstat(root fd) is not a directory");
    Outcome::Pass
}

fn mount_root_fstatvfs(ctx: &TestCtx) -> Outcome {
    let f = check_ok!(fsutil::open_dir(ctx.root()));
    let st = check_ok!(fsutil::fstatvfs(&f));
    check!(st.f_bsize > 0, "root f_bsize");
    check!(st.f_blocks > 0, "root f_blocks");
    check!(st.f_bfree <= st.f_blocks, "root f_bfree <= f_blocks");
    Outcome::Pass
}
