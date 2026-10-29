// Same as redoxfs: epoll_ctl is EPERM, relibc select treats that as ready.

use crate::{Outcome, TestCase, TestCtx, cases, check, check_errno, check_ok, check_out, fsutil};

const SELECT_MS: i32 = 200;

pub fn cases() -> Vec<TestCase> {
    cases![
        func fevent_epoll_and_select,
        func fevent_handles,
        edge edge_fevent_poll_eperm,
    ]
}

fn expect_epoll_eperm(file: &std::fs::File, what: &str) -> Result<(), Outcome> {
    match fsutil::epoll_add(file, libc::EPOLLIN | libc::EPOLLOUT) {
        Ok(()) => Err(Outcome::Fail(format!(
            "{what}: epoll_ctl succeeded, expected EPERM"
        ))),
        Err(err) if fsutil::is_errno(&err, fsutil::EPERM) => Ok(()),
        Err(err) => Err(Outcome::Fail(format!(
            "{what}: expected EPERM, got {err:?}"
        ))),
    }
}

fn fevent_epoll_and_select(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fevent.bin");
    check_ok!(fsutil::write_all_path(&path, b"ready"));
    let rw = check_ok!(fsutil::open_rw(&path));
    check_out!(expect_epoll_eperm(&rw, "epoll_ctl file"));
    check!(
        check_ok!(fsutil::select_writable(&rw, SELECT_MS)),
        "select did not report writable"
    );
    check!(
        check_ok!(fsutil::select_readable(&rw, 0)),
        "select(0) should see a regular file as ready"
    );
    let ro = check_ok!(fsutil::open_ro(&path));
    check!(
        check_ok!(fsutil::select_readable(&ro, SELECT_MS)),
        "select did not report readable"
    );
    check_out!(expect_epoll_eperm(&ro, "epoll_ctl RO"));
    Outcome::Pass
}

fn fevent_handles(ctx: &TestCtx) -> Outcome {
    let file = ctx.work("fevent_h.bin");
    check_ok!(fsutil::write_all_path(&file, b"data"));
    let dirp = ctx.work("fevent_dir");
    check_ok!(fsutil::make_dir(&dirp));
    let tgt = ctx.work("fevent_link_tgt.bin");
    let link = ctx.work("fevent_link");
    check_ok!(fsutil::write_all_path(&tgt, b"t"));
    check_ok!(fsutil::symlink("fevent_link_tgt.bin", &link));

    let empty = ctx.work("fevent_empty.bin");
    check_ok!(fsutil::write_all_path(&empty, b""));

    let unlinked = ctx.work("fevent_unlinked.bin");
    check_ok!(fsutil::write_all_path(&unlinked, b"t"));
    let un_fd = check_ok!(fsutil::open_rw(&unlinked));
    check_ok!(fsutil::unlink(&unlinked));

    let handles: [(&str, std::fs::File); 7] = [
        ("file", check_ok!(fsutil::open_rw(&file))),
        ("dir", check_ok!(fsutil::open_dir(&dirp))),
        ("O_STAT", check_ok!(fsutil::open_stat(&file))),
        ("symlink", check_ok!(fsutil::open_symlink(&link))),
        (
            "pending symlink",
            check_ok!(fsutil::open_creat_symlink(&ctx.work("fevent_presym"))),
        ),
        ("empty", check_ok!(fsutil::open_rw(&empty))),
        ("unlinked", un_fd),
    ];
    for (what, f) in &handles {
        check_out!(expect_epoll_eperm(f, &format!("epoll_ctl {what}")));
    }
    let dir = check_ok!(fsutil::open_dir(&dirp));
    check!(
        check_ok!(fsutil::select_readable(&dir, SELECT_MS)),
        "directory should be select-ready"
    );
    check!(
        check_ok!(fsutil::select_readable(&handles[6].1, SELECT_MS)),
        "unlinked fd should still be select-ready"
    );
    check!(
        check_ok!(fsutil::select_readable(&handles[5].1, SELECT_MS)),
        "empty file should still be select-ready"
    );
    Outcome::Pass
}

fn edge_fevent_poll_eperm(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fevent_poll.bin");
    check_ok!(fsutil::write_all_path(&path, b"t"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_errno!(fsutil::poll_in(&f, 0), fsutil::EPERM, "poll on file");
    Outcome::Pass
}
