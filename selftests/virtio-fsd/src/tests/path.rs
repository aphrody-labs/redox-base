use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_bytes, check_errno, check_ok, check_out, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func path_nested_lookup,
        func path_nested_fpath,
        func path_openat_dotdot,
        func path_openat_dotdot_to_sibling,
        func path_root_dotdot_stays_in_scheme,
        func path_component_through_file_is_enotdir,
        func path_deep_chain_fpath,
    ]
}

fn path_nested_lookup(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let leaf = ctx.fixtures().join("nested/a/b/c/leaf.txt");
    let data = check_ok!(fsutil::read_to_vec(&leaf));
    check!(
        data.windows(b"leaf".len()).any(|w| w == b"leaf"),
        "leaf.txt unexpected: {data:?}"
    );
    Outcome::Pass
}

fn path_nested_fpath(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let f = check_ok!(fsutil::open_ro(
        &ctx.fixtures().join("nested/a/b/c/leaf.txt")
    ));
    let p = check_ok!(fsutil::fpath(&f));
    check!(
        p.contains("nested") && p.contains("leaf.txt") && !p.contains(".."),
        "fpath should be canonical nested path, got {p}"
    );
    Outcome::Pass
}

fn path_openat_dotdot(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let dir = check_ok!(fsutil::open_dir(&ctx.fixtures().join("nested/a")));
    let parent = check_ok!(fsutil::openat_dir(&dir, ".."));
    let p = check_ok!(fsutil::fpath(&parent));
    check!(p.contains("nested"), "openat('..') fpath unexpected: {p}");
    Outcome::Pass
}

fn path_openat_dotdot_to_sibling(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let dir = check_ok!(fsutil::open_dir(&ctx.fixtures().join("nested")));
    let mut f = check_ok!(fsutil::openat_ro(&dir, "../files/small.txt"));
    let mut buf = String::new();
    check_ok!(std::io::Read::read_to_string(&mut f, &mut buf));
    check!(buf.contains("small"), "sibling content unexpected: {buf:?}");
    Outcome::Pass
}

fn path_root_dotdot_stays_in_scheme(ctx: &TestCtx) -> Outcome {
    let root = check_ok!(fsutil::open_dir(ctx.root()));
    let up = check_ok!(fsutil::openat_dir(&root, ".."));
    let p = check_ok!(fsutil::fpath(&up));
    check!(
        p.starts_with("/scheme/"),
        "openat('..') left the scheme: {p}"
    );
    let root_str = ctx.root().to_string_lossy();
    if let Some(scheme) = scheme_root_of(&root_str) {
        if root_str.trim_end_matches('/') == scheme {
            check!(
                p.trim_end_matches('/') == scheme,
                "scheme-root/.. should stay at scheme root: {p}"
            );
        }
    }
    Outcome::Pass
}

fn scheme_root_of(path: &str) -> Option<String> {
    let rest = path.trim_end_matches('/').strip_prefix("/scheme/")?;
    let name = rest.split('/').next()?;
    Some(format!("/scheme/{name}"))
}

fn path_component_through_file_is_enotdir(ctx: &TestCtx) -> Outcome {
    let dir = ctx.work("path_file_comp");
    check_ok!(fsutil::make_dir(&dir));
    check_ok!(fsutil::write_all_path(&dir.join("f"), b"x"));
    let parent = check_ok!(fsutil::open_dir(&dir));
    // `f/..` must not open `dir`. `f/.` must not open `f`.
    check_errno!(
        fsutil::openat_dir(&parent, "f/.."),
        fsutil::ENOTDIR,
        "file/.."
    );
    check_errno!(fsutil::openat_ro(&parent, "f/."), fsutil::ENOTDIR, "file/.");
    check_errno!(fsutil::openat_ro(&parent, "f/x"), fsutil::ENOTDIR, "file/x");
    check_bytes!(check_ok!(fsutil::read_to_vec(&dir.join("f"))), b"x");
    let up = check_ok!(fsutil::openat_dir(&parent, ".."));
    let p = check_ok!(fsutil::fpath(&up));
    check!(
        p.starts_with("/scheme/") && !p.contains("path_file_comp"),
        "looking up f retargeted the directory: {p}"
    );
    Outcome::Pass
}

fn path_deep_chain_fpath(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let dir = check_ok!(fsutil::open_dir(&ctx.fixtures().join("nested/a/b/c")));
    let p = check_ok!(fsutil::fpath(&dir));
    for component in ["nested", "a", "b", "c"] {
        check!(p.contains(component), "fpath missing `{component}`: {p}");
    }
    check!(!p.contains(".."), "deep fpath not canonical: {p}");
    Outcome::Pass
}
