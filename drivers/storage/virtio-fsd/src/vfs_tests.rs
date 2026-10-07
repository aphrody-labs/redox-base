use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use fuse_abi::linux::{
    fuse_attr, fuse_attr_out, fuse_entry_out, fuse_opcode, fuse_open_out, fuse_statfs_out,
    fuse_write_out,
};
use futures::executor::{LocalPool, block_on};
use futures::task::LocalSpawnExt;
use syscall::{ENOENT, ESTALE, MODE_DIR, MODE_FILE, O_APPEND, O_CREAT, O_RDONLY, O_RDWR};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::*;
use crate::fuse::{
    Caller, Data, FUSE_ROOT_ID, FuseError, FuseReply, FuseRequest, FuseSession, FuseTransporter,
};
use crate::linux_abi;

#[derive(Clone)]
struct Mock {
    replies: Rc<RefCell<VecDeque<MockReply>>>,
    ops: Rc<RefCell<Vec<u32>>>,
    // (opcode, header uid) of every request sent through `send`.
    uids: Rc<RefCell<Vec<(u32, u32)>>>,
    yield_once: Rc<Cell<bool>>,
    // When set, the first `FUSE_READ` stays inside `send` until a second read enters.
    gate_reads: Rc<Cell<bool>>,
    reads_entered: Rc<Cell<usize>>,
}

enum MockReply {
    Bytes(Vec<u8>),
    Unit,
    Host(i32),
}

impl Mock {
    fn new(replies: impl IntoIterator<Item = MockReply>) -> Self {
        Self {
            replies: Rc::new(RefCell::new(replies.into_iter().collect())),
            ops: Rc::new(RefCell::new(Vec::new())),
            uids: Rc::new(RefCell::new(Vec::new())),
            yield_once: Rc::new(Cell::new(false)),
            gate_reads: Rc::new(Cell::new(false)),
            reads_entered: Rc::new(Cell::new(0)),
        }
    }

    fn ops(&self) -> Vec<u32> {
        self.ops.borrow().clone()
    }

    fn assert_sent(&self, op: fuse_opcode) {
        let want = u32::from(op);
        let ops = self.ops();
        assert!(ops.contains(&want), "expected {want}, got {ops:?}");
    }
}

async fn yield_once() {
    let mut done = false;
    std::future::poll_fn(|cx| {
        if done {
            Poll::Ready(())
        } else {
            done = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

fn entry(nodeid: u64, generation: u64, mode: u32) -> fuse_entry_out {
    fuse_entry_out {
        nodeid,
        generation,
        entry_valid: 60,
        attr_valid: 60,
        attr: fuse_attr {
            ino: nodeid,
            mode,
            nlink: 1,
            blksize: 4096,
            ..fuse_attr::default()
        },
        ..fuse_entry_out::default()
    }
}

fn bytes(payload: &(impl IntoBytes + Immutable)) -> MockReply {
    MockReply::Bytes(payload.as_bytes().to_vec())
}

fn file(nodeid: u64, generation: u64) -> MockReply {
    bytes(&entry(nodeid, generation, MODE_FILE as u32))
}

fn dir(nodeid: u64, generation: u64) -> MockReply {
    bytes(&entry(nodeid, generation, MODE_DIR as u32))
}

fn open_out(fh: u64) -> fuse_open_out {
    fuse_open_out {
        fh,
        open_flags: 0,
        backing_id: 0,
    }
}

fn create_reply(nodeid: u64, generation: u64, fh: u64) -> MockReply {
    let mut payload = entry(nodeid, generation, MODE_FILE as u32)
        .as_bytes()
        .to_vec();
    payload.extend_from_slice(open_out(fh).as_bytes());
    MockReply::Bytes(payload)
}

impl FuseTransporter for Mock {
    async fn send<I, O>(
        &self,
        request: FuseRequest<I>,
        data: Data<'_>,
    ) -> Result<FuseReply<O>, FuseError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable,
    {
        if self.yield_once.get() {
            yield_once().await;
        }
        let opcode = request.fuse_in_header.opcode;
        if self.gate_reads.get() && opcode == u32::from(fuse_opcode::FUSE_READ) {
            let n = self.reads_entered.get() + 1;
            self.reads_entered.set(n);
            if n == 1 {
                yield_once().await;
                assert!(
                    self.reads_entered.get() >= 2,
                    "second FUSE_READ did not enter send while the first was in flight"
                );
            }
        }
        self.ops.borrow_mut().push(opcode);
        self.uids
            .borrow_mut()
            .push((opcode, request.fuse_in_header.uid));
        let reply = self
            .replies
            .borrow_mut()
            .pop_front()
            .ok_or(FuseError::MalformedPayload)?;
        match reply {
            MockReply::Host(errno) => Err(FuseError::Host(errno)),
            MockReply::Unit => Ok(FuseReply::Struct(
                O::read_from_bytes(&[]).map_err(|_| FuseError::MalformedPayload)?,
            )),
            MockReply::Bytes(buf) => match &data {
                Data::In(dst) => Ok(FuseReply::Size(dst.len())),
                _ => Ok(FuseReply::Struct(
                    O::read_from_prefix(&buf)
                        .map(|(s, _)| s)
                        .or_else(|_| O::read_from_bytes(&buf))
                        .map_err(|_| FuseError::MalformedPayload)?,
                )),
            },
        }
    }

    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), FuseError>
    where
        I: IntoBytes + Immutable,
    {
        self.ops.borrow_mut().push(request.fuse_in_header.opcode);
        Ok(())
    }
}

fn vfs_with(replies: impl IntoIterator<Item = MockReply>) -> (Vfs<Mock>, Mock) {
    let mock = Mock::new(replies);
    (Vfs::new(FuseSession::for_test(mock.clone())), mock)
}

fn has_inode(vfs: &Vfs<Mock>, id: u64) -> bool {
    vfs.state.borrow().inodes.contains_key(&id)
}

fn forget_queued(vfs: &Vfs<Mock>, id: u64) -> bool {
    vfs.state
        .borrow()
        .forget_queue
        .iter()
        .any(|(n, _)| *n == id)
}

async fn lookup(vfs: &Vfs<Mock>, name: &str) -> u64 {
    vfs.lookup(FUSE_ROOT_ID, name, Caller::NONE)
        .await
        .unwrap()
        .0
}

fn file_walk(nodeid: u64, name: &str) -> WalkResult {
    WalkResult {
        nodeid,
        node_mode: MODE_FILE,
        parent_nodeid: FUSE_ROOT_ID,
        rel_path: vec![name.into()],
        last_to_be_created: false,
    }
}

// Root + 7 files fills `INODES_MAX_LEN` (8 in tests).
fn fill_then(tail: impl IntoIterator<Item = MockReply>) -> Vec<MockReply> {
    let mut replies: Vec<_> = (2..=9).map(|i| file(i, 1)).collect();
    replies.extend(tail);
    replies
}

#[test]
fn create_rolls_back_host_file_on_generation_mismatch() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        create_reply(10, 2, 7),
        MockReply::Unit, // RELEASE
        MockReply::Unit, // UNLINK
    ]);

    block_on(async {
        assert_eq!(lookup(&vfs, "f").await, 10);
        assert!(has_inode(&vfs, 10));
        let err = vfs
            .create(
                FUSE_ROOT_ID,
                "f",
                O_CREAT | O_RDWR,
                MODE_FILE as u32,
                vec!["f".into()],
                Caller::NONE,
            )
            .await
            .expect_err("create must fail on generation mismatch");
        assert_eq!(err.errno, ESTALE);
    });

    mock.assert_sent(fuse_opcode::FUSE_RELEASE);
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
    // Failure path does not flush. the extra lookup ref must still be queued.
    assert!(
        forget_queued(&vfs, 10),
        "generation mismatch must queue FUSE_FORGET"
    );
}

#[test]
fn rm_sends_unlink_after_inode_eviction() {
    let (vfs, mock) = vfs_with(fill_then([MockReply::Unit]));
    block_on(async {
        lookup(&vfs, "victim").await;
        for i in 3..=9 {
            lookup(&vfs, &format!("f{i}")).await;
        }
        assert!(
            !has_inode(&vfs, 2),
            "victim should have been LRU-evicted, cache len={}",
            vfs.state.borrow().inodes.len()
        );
        vfs.rm(file_walk(2, "victim"), false, Caller::NONE)
            .await
            .expect("rm must not fail with ENOENT after eviction");
    });
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

#[test]
fn rm_forgets_the_removed_inode_before_returning() {
    let (vfs, mock) = vfs_with([file(10, 1), MockReply::Unit]);

    block_on(async {
        let id = lookup(&vfs, "f").await;
        vfs.rm(file_walk(id, "f"), false, Caller::NONE)
            .await
            .unwrap();
    });

    assert!(!has_inode(&vfs, 10));
    assert!(
        !forget_queued(&vfs, 10),
        "rm must not leave the forget queued for a later operation"
    );
    assert_eq!(
        mock.ops(),
        [
            u32::from(fuse_opcode::FUSE_LOOKUP),
            u32::from(fuse_opcode::FUSE_UNLINK),
            u32::from(fuse_opcode::FUSE_FORGET),
        ]
    );
}

#[test]
fn lookup_getattr_refresh_does_not_return_forgotten_nodeid() {
    let (vfs, _) = vfs_with([file(4, 1), MockReply::Host(ENOENT)]);
    block_on(async {
        assert_eq!(lookup(&vfs, "stale").await, 4);
        vfs.state
            .borrow_mut()
            .inodes
            .get_mut(&4)
            .unwrap()
            .attr_expiry = Instant::now() - Duration::from_secs(1);
        let err = vfs
            .lookup(FUSE_ROOT_ID, "stale", Caller::NONE)
            .await
            .expect_err("ENOENT getattr must not return the stale nodeid");
        assert_eq!(err.errno, ENOENT);
    });
}

#[test]
fn interleaved_lookup_cannot_evict_pinned_unlink_target() {
    let (vfs, mock) = vfs_with(fill_then([MockReply::Unit]));
    mock.yield_once.set(true);
    let vfs = Rc::new(vfs);

    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                let id = lookup(&vfs, "victim").await;
                vfs.rm(file_walk(id, "victim"), false, Caller::NONE)
                    .await
                    .unwrap();
            }
        })
        .unwrap();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                for i in 3..=8 {
                    let _ = vfs
                        .lookup(FUSE_ROOT_ID, &format!("f{i}"), Caller::NONE)
                        .await;
                }
            }
        })
        .unwrap();

    pool.run();
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

#[test]
fn mkdir_uses_rollback_helper_on_merge_failure() {
    let (vfs, mock) = vfs_with([dir(11, 1), dir(11, 2), MockReply::Unit]);
    block_on(async {
        lookup(&vfs, "d").await;
        let err = vfs
            .mkdir(FUSE_ROOT_ID, "d", MODE_DIR as u32, Caller::NONE)
            .await
            .expect_err("mkdir generation mismatch");
        assert_eq!(err.errno, ESTALE);
    });
    mock.assert_sent(fuse_opcode::FUSE_RMDIR);
}

#[test]
fn link_rolls_back_on_generation_mismatch() {
    let (vfs, mock) = vfs_with([
        file(20, 1),
        bytes(&open_out(3)),
        MockReply::Host(ENOENT), // dest walk last component
        file(20, 2),             // FUSE_LINK
        MockReply::Unit,         // UNLINK
    ]);

    block_on(async {
        lookup(&vfs, "src").await;
        let fd = vfs
            .open(20, O_RDWR, false, vec!["src".into()], Caller::NONE)
            .await
            .unwrap();
        let err = vfs
            .link(fd, "/dst", Caller::NONE)
            .await
            .expect_err("link generation mismatch");
        assert_eq!(err.errno, ESTALE);
    });
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

#[test]
fn rename_keeps_parent_child_count() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        bytes(&open_out(1)),
        MockReply::Host(ENOENT),
        MockReply::Unit,
    ]);
    block_on(async {
        lookup(&vfs, "a").await;
        let fd = vfs
            .open(10, O_RDWR, false, vec!["a".into()], Caller::NONE)
            .await
            .unwrap();
        vfs.rename(fd, "/b", Caller::NONE).await.unwrap();
    });
    let g = vfs.state.borrow();
    assert_eq!(g.inodes[&FUSE_ROOT_ID].child_count, 1);
    assert!(g.dcache[&FUSE_ROOT_ID].get("a").is_none());
    assert_eq!(g.dcache[&FUSE_ROOT_ID]["b"].nodeid, 10);
    drop(g);
    mock.assert_sent(fuse_opcode::FUSE_RENAME);
}

#[test]
fn symlink_rollback_drops_committed_dentry() {
    let (vfs, mock) = vfs_with([file(30, 1), MockReply::Unit]);
    mock.yield_once.set(true);
    let fd = vfs
        .pre_create_symlink(
            FUSE_ROOT_ID,
            "link".into(),
            vec!["link".into()],
            Caller::NONE,
        )
        .unwrap();
    let vfs = Rc::new(vfs);

    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                let err = vfs
                    .write(fd, b"target", 0, O_RDWR as u32)
                    .await
                    .expect_err("fd closed during symlink");
                assert_eq!(err.errno, syscall::EBADF);
            }
        })
        .unwrap();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                vfs.close(fd, Caller::NONE).await;
            }
        })
        .unwrap();
    pool.run();

    assert!(!has_inode(&vfs, 30));
    let g = vfs.state.borrow();
    assert!(
        g.dcache
            .get(&FUSE_ROOT_ID)
            .and_then(|names| names.get("link"))
            .is_none()
    );
    assert_eq!(g.inodes[&FUSE_ROOT_ID].child_count, 0);
    drop(g);
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

#[test]
fn concurrent_reads_overlap_in_send() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        bytes(&open_out(1)),
        MockReply::Bytes(vec![0xAB]),
        MockReply::Bytes(vec![0xAB]),
    ]);
    mock.gate_reads.set(true);
    let vfs = Rc::new(vfs);
    let fd = block_on(async {
        lookup(&vfs, "f").await;
        vfs.open(10, O_RDONLY, false, vec!["f".into()], Caller::NONE)
            .await
            .unwrap()
    });

    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let done = Rc::new(Cell::new(0usize));
    for _ in 0..2 {
        let vfs = Rc::clone(&vfs);
        let done = Rc::clone(&done);
        spawner
            .spawn_local(async move {
                let mut buf = [0u8; 8];
                let n = vfs.read(fd, &mut buf, 0, O_RDONLY as u32).await.unwrap();
                assert_eq!(n, 8);
                done.set(done.get() + 1);
            })
            .unwrap();
    }
    pool.run();
    assert_eq!(done.get(), 2);
    assert!(mock.reads_entered.get() >= 2);
}

const OPENER: Caller = Caller {
    uid: 1000,
    gid: 1000,
    pid: 7,
};

/// Opens a file as `OPENER`, runs `op` as another user and returns the uid of the single
/// `opcode` request it sent. `reply` answers that request.
fn uid_on_open_handle(
    reply: MockReply,
    opcode: fuse_opcode,
    op: impl AsyncFnOnce(&Vfs<Mock>, usize, Caller),
) -> u32 {
    let other = Caller {
        uid: 0,
        gid: 0,
        pid: 8,
    };
    // The trailing reply is for the RELEASE that `close` sends after FLUSH.
    let (vfs, mock) = vfs_with([file(10, 1), bytes(&open_out(1)), reply, MockReply::Unit]);
    block_on(async {
        lookup(&vfs, "f").await;
        let fd = vfs
            .open(10, O_RDWR, false, vec!["f".into()], OPENER)
            .await
            .unwrap();
        op(&vfs, fd, other).await;
    });

    let uids = mock.uids.borrow();
    let mut sent = uids.iter().filter(|(op, _)| *op == u32::from(opcode));
    let (_, uid) = sent.next().expect("request was not sent");
    assert!(sent.next().is_none(), "request was sent more than once");
    *uid
}

#[test]
fn read_carries_the_opener() {
    let uid = uid_on_open_handle(
        MockReply::Bytes(vec![0xAB]),
        fuse_opcode::FUSE_READ,
        async |vfs, fd, _| {
            vfs.read(fd, &mut [0u8; 1], 0, O_RDWR as u32).await.unwrap();
        },
    );
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn write_carries_the_opener() {
    let reply = bytes(&fuse_write_out {
        size: 1,
        padding: 0,
    });
    let uid = uid_on_open_handle(reply, fuse_opcode::FUSE_WRITE, async |vfs, fd, _| {
        vfs.write(fd, b"x", 0, O_RDWR as u32).await.unwrap();
    });
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn ftruncate_carries_the_opener() {
    let reply = bytes(&fuse_attr_out::default());
    let uid = uid_on_open_handle(reply, fuse_opcode::FUSE_SETATTR, async |vfs, fd, _| {
        vfs.ftruncate(fd, 0).await.unwrap();
    });
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn statfs_carries_the_opener() {
    let reply = bytes(&fuse_statfs_out::default());
    let uid = uid_on_open_handle(reply, fuse_opcode::FUSE_STATFS, async |vfs, fd, _| {
        vfs.statfs(fd).await.unwrap();
    });
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn fsync_carries_the_opener() {
    let uid = uid_on_open_handle(
        MockReply::Unit,
        fuse_opcode::FUSE_FSYNC,
        async |vfs, fd, _| {
            vfs.fsync(fd).await.unwrap();
        },
    );
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn close_flush_carries_the_opener() {
    let uid = uid_on_open_handle(
        MockReply::Unit,
        fuse_opcode::FUSE_FLUSH,
        async |vfs, fd, other| vfs.close(fd, other).await,
    );
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn close_release_carries_the_opener() {
    let uid = uid_on_open_handle(
        MockReply::Unit,
        fuse_opcode::FUSE_RELEASE,
        async |vfs, fd, other| vfs.close(fd, other).await,
    );
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn append_is_per_write_not_open() {
    let open = fuse_open_flags(O_RDWR | O_APPEND, false, false).unwrap();
    assert_eq!(open & linux_abi::O_APPEND, 0);
    let io = fuse_io_flags((O_RDWR | O_APPEND) as u32);
    assert_ne!(io & linux_abi::O_APPEND, 0);
}
