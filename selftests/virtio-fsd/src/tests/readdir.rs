use crate::{Outcome, TestCase, TestCtx, cases, check, check_ok, check_out, fsutil};

pub fn cases() -> Vec<TestCase> {
    cases![
        func readdir_fixtures,
        func readdir_empty_dir,
        func readdir_many_files,
        func readdir_sees_created_file,
    ]
}

fn readdir_fixtures(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let names = check_ok!(fsutil::read_dir_names(&ctx.fixtures()));
    for expected in [
        "known.txt",
        "nested",
        "files",
        "empty",
        "many",
        "deletable",
        "links",
        "cp",
    ] {
        check!(
            names.iter().any(|n| n == expected),
            "fixtures/ missing `{expected}`; got {names:?}"
        );
    }
    Outcome::Pass
}

fn readdir_empty_dir(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let names = check_ok!(fsutil::read_dir_names(&ctx.fixtures().join("empty")));
    let extra: Vec<_> = names
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect();
    check!(extra.is_empty(), "empty/ has unexpected entries {extra:?}");
    Outcome::Pass
}

fn readdir_many_files(ctx: &TestCtx) -> Outcome {
    check_out!(ctx.require_fixtures());
    let names = check_ok!(fsutil::read_dir_names(&ctx.fixtures().join("many")));
    let files: Vec<_> = names
        .into_iter()
        .filter(|n| n.starts_with("file_"))
        .collect();
    check!(
        files.len() >= 200,
        "expected >=200 files, got {}",
        files.len()
    );
    check!(files.iter().any(|n| n == "file_000"), "missing file_000");
    check!(files.iter().any(|n| n == "file_199"), "missing file_199");
    Outcome::Pass
}

fn readdir_sees_created_file(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("readdir_visible.txt");
    let name = fsutil::base_name(&path).to_owned();
    check_ok!(fsutil::write_all_path(&path, b"visible"));
    let names = check_ok!(fsutil::read_dir_names(ctx.root()));
    check!(
        names.iter().any(|n| n == &name),
        "created file `{name}` not in readdir: {names:?}"
    );
    Outcome::Pass
}
