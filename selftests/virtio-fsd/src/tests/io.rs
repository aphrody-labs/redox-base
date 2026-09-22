// SEEK_END goes through scheme fsize, not fstat.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{
    Outcome, TestCase, TestCtx, cases, check, check_eq, check_errno, check_ok, check_out, fsutil,
};

pub fn cases() -> Vec<TestCase> {
    cases![
        func io_write_read_roundtrip,
        func io_empty_read,
        func io_partial_read,
        func io_positioned_write_read,
        func io_overwrite_middle,
        func io_large_buffer,
        func io_chunked_write_read,
        func io_chunked_unaligned,
        func io_chunked_overwrite_span,
        func io_chunked_read_span,
        func io_read_past_eof,
        func io_seek_end_empty,
        func io_seek_end_matches_len,
        func io_seek_end_then_read,
        func io_seek_end_relative,
        func io_seek_end_after_write,
        func io_seek_end_after_truncate,
        func io_seek_end_extends_on_write,
        func io_seek_end_overwrite_keeps_size,
        func io_seek_end_large,
        func io_seek_end_past_eof,
        func io_seek_end_through_symlink,
        func io_seek_end_wronly,
        func io_empty_buf_read,
        func io_empty_buf_write,
        func io_pread_pwrite,
        edge edge_seek_end_handles,
    ]
}

fn io_write_read_roundtrip(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_roundtrip.bin");
    let payload = b"abcdefghijklmnopqrstuvwxyz0123456789";
    check_ok!(fsutil::write_all_path(&path, payload));
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), payload.as_slice());
    Outcome::Pass
}

fn io_empty_read(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_empty_read.txt");
    check_ok!(fsutil::write_all_path(&path, b""));
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.len(), 0);
    Outcome::Pass
}

fn io_partial_read(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_partial.txt");
    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let mut f = check_ok!(fsutil::open_ro(&path));
    let mut buf = [0u8; 4];
    let n = check_ok!(f.read(&mut buf));
    check_eq!(n, 4);
    check_eq!(&buf, b"0123");
    Outcome::Pass
}

fn io_positioned_write_read(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_positioned.bin");
    check_ok!(fsutil::write_all_path(&path, &[0u8; 32]));
    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::write_at(&mut f, 8, b"ABCD"));
    check_ok!(fsutil::write_at(&mut f, 16, b"WXYZ"));
    let a = check_ok!(fsutil::read_at(&mut f, 8, 4));
    check_eq!(a.as_slice(), b"ABCD");
    let b = check_ok!(fsutil::read_at(&mut f, 16, 4));
    check_eq!(b.as_slice(), b"WXYZ");
    Outcome::Pass
}

fn io_overwrite_middle(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_overwrite.txt");
    check_ok!(fsutil::write_all_path(&path, b"XXXXXXXXXX"));
    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::write_at(&mut f, 3, b"YES"));
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"XXXYESXXXX");
    Outcome::Pass
}

fn io_large_buffer(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_large.bin");
    let size = 256 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    check_ok!(fsutil::write_all_path(&path, &data));
    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(got == data, "large buffer mismatch");
    Outcome::Pass
}

fn io_pattern(i: usize) -> u8 {
    ((i.wrapping_mul(131)) ^ (i >> 8) ^ (i >> 16)) as u8
}

fn io_patterned(len: usize) -> Vec<u8> {
    (0..len).map(io_pattern).collect()
}

fn io_chunked_write_read(ctx: &TestCtx) -> Outcome {
    // Bigger than virtiofsd's usual 1 MiB max_write / 128 KiB max_pages.
    let len = 2 * 1024 * 1024 + 4097;
    let path = ctx.work("io_chunk.bin");
    let data = io_patterned(len);
    check_ok!(fsutil::write_all_path(&path, &data));
    let mut f = check_ok!(fsutil::open_ro(&path));
    let got = check_ok!(fsutil::read_exact_at(&mut f, 0, len));
    check!(got == data, "chunked roundtrip mismatch");
    Outcome::Pass
}

fn io_chunked_unaligned(ctx: &TestCtx) -> Outcome {
    let len = 1024 * 1024 + 17;
    let path = ctx.work("io_chunk_unalign.bin");
    let data = io_patterned(len);
    check_ok!(fsutil::write_all_path(&path, &data));
    let mut f = check_ok!(fsutil::open_ro(&path));
    let got = check_ok!(fsutil::read_exact_at(&mut f, 0, len));
    check!(got == data, "unaligned chunked mismatch");
    Outcome::Pass
}

fn io_chunked_overwrite_span(ctx: &TestCtx) -> Outcome {
    let len = 2 * 1024 * 1024;
    let path = ctx.work("io_chunk_ow.bin");
    let mut data = io_patterned(len);
    check_ok!(fsutil::write_all_path(&path, &data));
    let at = 1024 * 1024 - 40;
    let patch: Vec<u8> = (0..80).map(|i| 0xA5 ^ i as u8).collect();
    data[at..at + patch.len()].copy_from_slice(&patch);
    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::write_at(&mut f, at as u64, &patch));
    let got = check_ok!(fsutil::read_exact_at(&mut f, 0, len));
    check!(got == data, "span overwrite mismatch");
    Outcome::Pass
}

fn io_chunked_read_span(ctx: &TestCtx) -> Outcome {
    let len = 2 * 1024 * 1024;
    let path = ctx.work("io_chunk_rdspan.bin");
    let data = io_patterned(len);
    check_ok!(fsutil::write_all_path(&path, &data));
    let mut f = check_ok!(fsutil::open_ro(&path));
    let at = 1024 * 1024 - 64;
    let n = 128;
    let got = check_ok!(fsutil::read_exact_at(&mut f, at as u64, n));
    check_eq!(got.as_slice(), &data[at..at + n]);
    Outcome::Pass
}

fn io_read_past_eof(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_eof.txt");
    check_ok!(fsutil::write_all_path(&path, b"short"));
    let mut f = check_ok!(fsutil::open_ro(&path));
    let past = check_ok!(fsutil::read_at(&mut f, 100, 16));
    check_eq!(past.len(), 0);
    Outcome::Pass
}

fn open_seeded(
    ctx: &TestCtx,
    name: &str,
    body: &[u8],
    open: fn(&Path) -> io::Result<File>,
) -> Result<File, Outcome> {
    let path = ctx.work(name);
    fsutil::write_all_path(&path, body).map_err(|e| Outcome::Fail(format!("seed {name}: {e}")))?;
    open(&path).map_err(|e| Outcome::Fail(format!("open {name}: {e}")))
}

fn seed_ro(ctx: &TestCtx, name: &str, body: &[u8]) -> Result<File, Outcome> {
    open_seeded(ctx, name, body, fsutil::open_ro)
}

fn seed_rw(ctx: &TestCtx, name: &str, body: &[u8]) -> Result<File, Outcome> {
    open_seeded(ctx, name, body, fsutil::open_rw)
}

fn seed_wo(ctx: &TestCtx, name: &str, body: &[u8]) -> Result<File, Outcome> {
    open_seeded(ctx, name, body, fsutil::open_wo)
}

fn io_seek_end_empty(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_rw(ctx, "io_seek_end_empty.bin", b""));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 0);
    Outcome::Pass
}

fn io_seek_end_matches_len(ctx: &TestCtx) -> Outcome {
    let payload = b"abcdefghijklmnopqrstuvwxyz";
    let mut f = check_out!(seed_ro(ctx, "io_seek_end_len.bin", payload));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), payload.len() as u64);
    check_eq!(
        check_ok!(fsutil::fstat(&f)).st_size as u64,
        payload.len() as u64
    );
    Outcome::Pass
}

fn io_seek_end_then_read(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_ro(ctx, "io_seek_end_read.bin", b"payload"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 7);
    let mut buf = [0u8; 8];
    let n = check_ok!(f.read(&mut buf));
    check_eq!(n, 0);
    Outcome::Pass
}

fn io_seek_end_relative(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_ro(ctx, "io_seek_end_rel.bin", b"0123456789"));
    check_eq!(check_ok!(f.seek(SeekFrom::End(-4))), 6);
    let mut buf = [0u8; 4];
    let n = check_ok!(f.read(&mut buf));
    check_eq!(n, 4);
    check_eq!(&buf, b"6789");
    Outcome::Pass
}

fn io_seek_end_after_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_seek_end_write.bin");
    let mut f = check_ok!(fsutil::create_rw(&path));
    check_ok!(f.write_all(b"abc"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 3);
    check_ok!(f.write_all(b"def"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 6);
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"abcdef");
    Outcome::Pass
}

fn io_seek_end_after_truncate(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_rw(ctx, "io_seek_end_trunc.bin", b"0123456789"));
    check_ok!(f.set_len(4));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 4);
    check_ok!(f.set_len(12));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 12);
    check_ok!(f.set_len(0));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 0);
    Outcome::Pass
}

fn io_seek_end_extends_on_write(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_rw(ctx, "io_seek_end_hole.bin", b"AB"));
    check_ok!(fsutil::write_at(&mut f, 8, b"YZ"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 10);
    Outcome::Pass
}

fn io_seek_end_overwrite_keeps_size(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_rw(ctx, "io_seek_end_ow.bin", b"XXXXXXXXXX"));
    check_ok!(fsutil::write_at(&mut f, 3, b"YES"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 10);
    Outcome::Pass
}

fn io_seek_end_large(ctx: &TestCtx) -> Outcome {
    let size = 256 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let mut f = check_out!(seed_ro(ctx, "io_seek_end_large.bin", &data));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), size as u64);
    Outcome::Pass
}

fn io_seek_end_past_eof(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_rw(ctx, "io_seek_end_past.bin", b"0123456789"));
    check_eq!(check_ok!(f.seek(SeekFrom::End(5))), 15);
    check_eq!(check_ok!(fsutil::fstat(&f)).st_size as u64, 10);
    check_ok!(f.write_all(b"X"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 16);
    Outcome::Pass
}

fn io_seek_end_through_symlink(ctx: &TestCtx) -> Outcome {
    let target = ctx.work("io_seek_end_follow_tgt.bin");
    let link = ctx.work("io_seek_end_follow_link");
    let payload = b"followed-size";
    check_ok!(fsutil::write_all_path(&target, payload));
    check_ok!(fsutil::symlink(fsutil::base_name(&target), &link));
    let mut f = check_ok!(fsutil::open_ro(&link));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), payload.len() as u64);
    Outcome::Pass
}

fn io_seek_end_wronly(ctx: &TestCtx) -> Outcome {
    let mut f = check_out!(seed_wo(ctx, "io_seek_end_wo.bin", b"0123456789"));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 10);
    Outcome::Pass
}

fn io_empty_buf_read(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_empty_buf_r.bin");
    check_ok!(fsutil::write_all_path(&path, b"data"));
    let mut f = check_ok!(fsutil::open_rw(&path));
    let n = check_ok!(f.read(&mut []));
    check_eq!(n, 0);
    Outcome::Pass
}

fn io_empty_buf_write(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_empty_buf_w.bin");
    check_ok!(fsutil::write_all_path(&path, b"data"));
    let mut f = check_ok!(fsutil::open_rw(&path));
    let n = check_ok!(f.write(&[]));
    check_eq!(n, 0);
    drop(f);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check_eq!(got.as_slice(), b"data");
    Outcome::Pass
}

fn io_pread_pwrite(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("io_pread.bin");
    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let f = check_ok!(fsutil::open_rw(&path));
    check_eq!(check_ok!(fsutil::pwrite(&f, 3, b"ABC")), 3);
    let got = check_ok!(fsutil::pread(&f, 2, 5));
    check_eq!(got.as_slice(), b"2ABC6");

    let mut f = f;
    let mut buf = [0u8; 2];
    check_eq!(check_ok!(f.read(&mut buf)), 2);
    check_eq!(&buf, b"01");
    Outcome::Pass
}

fn edge_seek_end_handles(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("edge_seek_end.bin");
    check_ok!(fsutil::write_all_path(&path, b"data"));
    let mut st = check_ok!(fsutil::open_stat(&path));
    check_eq!(check_ok!(fsutil::seek_end(&mut st)), 4);

    let dirp = ctx.work("edge_seek_end_dir");
    check_ok!(fsutil::make_dir(&dirp));
    let mut dir = check_ok!(fsutil::open_dir(&dirp));
    check_eq!(
        check_ok!(fsutil::seek_end(&mut dir)),
        check_ok!(fsutil::fstat(&dir)).st_size as u64
    );

    let dest = "edge_seek_end_link_tgt.bin";
    let tgt = ctx.work(dest);
    let link = ctx.work("edge_seek_end_link");
    check_ok!(fsutil::write_all_path(&tgt, b"0123456789"));
    check_ok!(fsutil::symlink(dest, &link));
    let mut sl = check_ok!(fsutil::open_symlink(&link));
    check_eq!(check_ok!(fsutil::seek_end(&mut sl)), dest.len() as u64);

    let mut pending = check_ok!(fsutil::open_creat_symlink(
        &ctx.work("edge_seek_end_presym")
    ));
    check_errno!(fsutil::seek_end(&mut pending), fsutil::EBADF, "pending");

    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let mut f = check_ok!(fsutil::open_rw(&path));
    check_ok!(fsutil::unlink(&path));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 10);
    check_ok!(f.set_len(4));
    check_eq!(check_ok!(fsutil::seek_end(&mut f)), 4);

    check_ok!(fsutil::write_all_path(&path, b"0123456789"));
    let a = check_ok!(fsutil::open_rw(&path));
    let mut b = check_ok!(fsutil::open_rw(&path));
    check_ok!(a.set_len(4));
    check_eq!(check_ok!(fsutil::seek_end(&mut b)), 4);

    check_ok!(fsutil::write_all_path(&path, b""));
    let mut f = check_ok!(fsutil::open_rw(&path));
    check_errno!(f.seek(SeekFrom::End(-1)), fsutil::EINVAL, "SEEK_END -1");
    Outcome::Pass
}
