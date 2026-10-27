use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use fuse_abi::linux::{
    fuse_attr, fuse_attr_out, fuse_entry_out, fuse_opcode, fuse_open_out, fuse_statfs_out,
    fuse_write_out,
};
use futures::executor::{LocalPool, block_on};
use futures::task::LocalSpawnExt;
use syscall::{
    EINVAL, ENOENT, ENOTDIR, EPERM, ESTALE, MODE_DIR, MODE_FILE, MODE_SYMLINK, O_APPEND, O_CREAT,
    O_EXCL, O_RDONLY, O_RDWR, O_TRUNC,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::*;
use crate::fuse::{Caller, Data, FuseError, FuseReply, FuseRequest, FuseSession, FuseTransporter};
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

fn has_inode(vfs: &Vfs<Mock>, raw: u64) -> bool {
    vfs.state.borrow().inodes.contains_key(&NodeId::new(raw))
}

fn forget_queued(vfs: &Vfs<Mock>, raw: u64) -> bool {
    vfs.state
        .borrow()
        .forget_queue
        .iter()
        .any(|(n, _)| *n == NodeId::new(raw))
}

async fn lookup(vfs: &Vfs<Mock>, name: &str) -> NodeId {
    vfs.lookup(NodeId::ROOT, name, Caller::NONE)
        .await
        .unwrap()
        .0
}

fn open_fds(vfs: &Vfs<Mock>, nodeid: NodeId) -> u64 {
    vfs.state.borrow().inodes[&nodeid].open_fd_count
}

fn file_entry<'a>(vfs: &'a Vfs<Mock>, nodeid: NodeId, name: &str) -> Entry<'a, Mock> {
    Entry {
        nodeid: Some(nodeid),
        node_pin: vfs.pin(nodeid).ok(),
        ..new_entry(vfs, name)
    }
}

/// A name in the root that does not exist yet, built without a lookup, so the cache can
/// hold a stale inode for it.
fn new_entry<'a>(vfs: &'a Vfs<Mock>, name: &str) -> Entry<'a, Mock> {
    Entry {
        parent: NodeId::ROOT,
        name: name.into(),
        nodeid: None,
        path: name.into(),
        _parent_pin: vfs.pin(NodeId::ROOT).unwrap(),
        node_pin: None,
    }
}

async fn rename_fd(vfs: &Vfs<Mock>, fd: usize, dest: &str) -> Result<()> {
    let src = vfs.entry_of_fd(fd, Caller::NONE).await?;
    let dest = vfs.entry_at(fd, dest, Caller::NONE).await?;
    vfs.rename(src, dest, 0, Caller::NONE).await
}

// Root + 7 files fills `INODES_MAX_LEN` (8 in tests).
fn fill_then(tail: impl IntoIterator<Item = MockReply>) -> Vec<MockReply> {
    let mut replies: Vec<_> = (2..=9).map(|i| file(i, 1)).collect();
    replies.extend(tail);
    replies
}

fn assert_lru_consistent(vfs: &Vfs<Mock>) {
    let s = vfs.state.borrow();
    let expected: BTreeSet<_> = s
        .inodes
        .iter()
        .filter(|(id, i)| VfsState::evictable(**id, i, s.dcache.contains_key(*id)))
        .map(|(id, i)| (i.lru_timestamp, *id))
        .collect();
    assert_eq!(s.lru_evictables, expected);
    for (dir, names) in &s.dcache {
        assert!(s.inodes.contains_key(dir), "dcache key {dir} is not cached");
        assert!(!names.is_empty(), "dcache keeps an empty map for {dir}");
    }
}

fn children(vfs: &Vfs<Mock>, dir: NodeId) -> usize {
    vfs.state
        .borrow()
        .dcache
        .get(&dir)
        .map_or(0, |names| names.len())
}

fn in_lru_evictables(vfs: &Vfs<Mock>, raw: u64) -> bool {
    vfs.state
        .borrow()
        .lru_evictables
        .iter()
        .any(|(_, id)| *id == NodeId::new(raw))
}

#[test]
fn lru_evictables_follows_open_fds_and_pins() {
    let (vfs, _) = vfs_with([file(2, 1)]);
    block_on(async {
        lookup(&vfs, "f").await;
        assert!(in_lru_evictables(&vfs, 2));
        assert_lru_consistent(&vfs);

        let id = NodeId::new(2);
        vfs.state.borrow_mut().increment_fd_count(id).unwrap();
        assert!(
            !in_lru_evictables(&vfs, 2),
            "open inode must not be evictable"
        );
        assert_lru_consistent(&vfs);

        vfs.state.borrow_mut().pin(id).unwrap();
        vfs.state.borrow_mut().decrement_fd_count(id);
        assert!(
            !in_lru_evictables(&vfs, 2),
            "pinned inode must not be evictable"
        );
        assert_lru_consistent(&vfs);

        vfs.state.borrow_mut().unpin(id);
        assert!(in_lru_evictables(&vfs, 2), "idle again, so evictable again");
        assert_lru_consistent(&vfs);

        vfs.state.borrow_mut().increment_fd_count(id).unwrap();
        vfs.state.borrow_mut().touch(id);
        assert!(!in_lru_evictables(&vfs, 2));
        assert_lru_consistent(&vfs);
    });
}

#[test]
fn lru_evictables_follows_children() {
    let (vfs, _) = vfs_with([dir(2, 1), file(3, 1)]);
    block_on(async {
        lookup(&vfs, "d").await;
        assert!(in_lru_evictables(&vfs, 2));
        vfs.lookup(NodeId::new(2), "c", Caller::NONE).await.unwrap();
        assert!(
            !in_lru_evictables(&vfs, 2),
            "a directory with a child is not evictable"
        );
        assert!(in_lru_evictables(&vfs, 3));
        assert_lru_consistent(&vfs);

        vfs.state.borrow_mut().remove_inode(NodeId::new(3));
        assert!(in_lru_evictables(&vfs, 2));
        assert_lru_consistent(&vfs);
    });
}

#[test]
fn cache_grows_past_limit_when_nothing_is_evictable() {
    let (vfs, _) = vfs_with((2..=11).map(|i| file(i, 1)));
    block_on(async {
        // Root + f2..f8 fill the cache and every one of them stays open.
        for i in 2..=8 {
            lookup(&vfs, &format!("f{i}")).await;
            vfs.state
                .borrow_mut()
                .increment_fd_count(NodeId::new(i))
                .unwrap();
        }
        assert!(vfs.state.borrow().lru_evictables.is_empty());
        assert!(!vfs.state.borrow().lru_full_warned);

        lookup(&vfs, "f9").await;
        assert!(vfs.state.borrow().lru_full_warned);
        vfs.state
            .borrow_mut()
            .increment_fd_count(NodeId::new(9))
            .unwrap();
        lookup(&vfs, "f10").await;
        assert_eq!(vfs.state.borrow().inodes.len(), 10);
        assert!(has_inode(&vfs, 2), "open inodes must survive");
        assert_lru_consistent(&vfs);

        for i in 2..=9 {
            vfs.state.borrow_mut().decrement_fd_count(NodeId::new(i));
        }
        lookup(&vfs, "f11").await;
        assert_eq!(vfs.state.borrow().inodes.len(), 8);
        assert!(!vfs.state.borrow().lru_full_warned);
        assert_lru_consistent(&vfs);
    });
}

#[test]
fn touching_an_old_inode_protects_it_from_eviction() {
    let (vfs, _) = vfs_with(fill_then([]));
    block_on(async {
        for i in 2..=8 {
            lookup(&vfs, &format!("f{i}")).await;
        }
        // f2 is the oldest. Touching it makes f3 the oldest.
        vfs.state.borrow_mut().touch(NodeId::new(2));
        assert_lru_consistent(&vfs);
        lookup(&vfs, "f9").await;
        assert!(has_inode(&vfs, 2), "touched inode must survive");
        assert!(!has_inode(&vfs, 3), "oldest untouched inode must go");
        assert_lru_consistent(&vfs);
    });
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
        assert_eq!(lookup(&vfs, "f").await, NodeId::new(10));
        assert!(has_inode(&vfs, 10));
        let err = vfs
            .create(
                new_entry(&vfs, "f"),
                O_CREAT | O_RDWR,
                MODE_FILE as u32,
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
fn create_releases_host_fh_without_a_scheme_handle() {
    let (vfs, mock) = vfs_with([create_reply(20, 1, 7), MockReply::Unit]);

    block_on(async {
        let (nodeid, mode) = vfs
            .create(
                new_entry(&vfs, "f"),
                O_CREAT | O_RDWR,
                MODE_FILE as u32,
                Caller::NONE,
            )
            .await
            .expect("create");
        assert_eq!(nodeid, NodeId::new(20));
        assert_eq!(mode, MODE_FILE);
        assert!(has_inode(&vfs, 20));
        assert_eq!(
            vfs.state.borrow().inodes[&NodeId::new(20)].open_fd_count,
            0,
            "create must not publish a scheme fd"
        );
    });

    mock.assert_sent(fuse_opcode::FUSE_CREATE);
    mock.assert_sent(fuse_opcode::FUSE_RELEASE);
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
        vfs.rm(
            file_entry(&vfs, NodeId::new(2), "victim"),
            false,
            Caller::NONE,
        )
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
        vfs.rm(file_entry(&vfs, id, "f"), false, Caller::NONE)
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
        assert_eq!(lookup(&vfs, "stale").await, NodeId::new(4));
        vfs.state
            .borrow_mut()
            .inodes
            .get_mut(&NodeId::new(4))
            .unwrap()
            .attr_expiry = Instant::now() - Duration::from_secs(1);
        let err = vfs
            .lookup(NodeId::ROOT, "stale", Caller::NONE)
            .await
            .expect_err("ENOENT getattr must not return the stale nodeid");
        assert_eq!(err.errno, ENOENT);
    });
}

#[test]
fn dotdot_of_a_directory_is_its_parent() {
    let (vfs, mock) = vfs_with([dir(11, 1)]);
    let root = vfs.scheme_root();
    block_on(async {
        lookup(&vfs, "d").await;
        let up = vfs
            .walk_path(root, "d/..", O_RDONLY, Caller::NONE)
            .await
            .unwrap();
        assert_eq!(up.nodeid, NodeId::ROOT);
        assert_eq!(up.node_mode, MODE_DIR);
        let here = vfs
            .walk_path(root, "d/.", O_RDONLY, Caller::NONE)
            .await
            .unwrap();
        assert_eq!(here.nodeid, NodeId::new(11));
    });
    assert_eq!(mock.ops(), vec![u32::from(fuse_opcode::FUSE_LOOKUP)]);
}

#[test]
fn entry_at_resolves_from_the_dir_handle_node() {
    let (vfs, mock) = vfs_with([dir(11, 1), bytes(&open_out(1)), MockReply::Host(ENOENT)]);
    block_on(async {
        lookup(&vfs, "d").await;
        let fd = vfs
            .open(
                NodeId::new(11),
                O_RDONLY,
                true,
                vec!["moved".into(), "away".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        let entry = vfs.entry_at(fd, "x", Caller::NONE).await.unwrap();
        assert_eq!(entry.parent, NodeId::new(11));
        assert_eq!(entry.name, "x");
        assert_eq!(entry.nodeid, None);
        assert_eq!(entry.path, "moved/away/x");
    });
    assert_eq!(
        mock.ops(),
        vec![
            u32::from(fuse_opcode::FUSE_LOOKUP),
            u32::from(fuse_opcode::FUSE_OPENDIR),
            u32::from(fuse_opcode::FUSE_LOOKUP),
        ]
    );
}

#[test]
fn entry_at_rejects_names_that_are_not_entries() {
    let (vfs, mock) = vfs_with([dir(11, 1)]);
    let root = vfs.scheme_root();
    block_on(async {
        lookup(&vfs, "d").await;
        for path in ["", "/", ".", "..", "d/..", "d/.", "d/../."] {
            let err = vfs
                .entry_at(root, path, Caller::NONE)
                .await
                .map(|_| ())
                .expect_err(path);
            assert_eq!(err.errno, EINVAL, "{path:?}");
        }
    });
    // Only the lookup of "d" above; the rest resolve from the cache.
    assert_eq!(mock.ops(), vec![u32::from(fuse_opcode::FUSE_LOOKUP)]);
}

#[test]
fn component_through_a_file_is_enotdir() {
    let (vfs, mock) = vfs_with([file(10, 1), dir(11, 1)]);
    let root = vfs.scheme_root();
    block_on(async {
        lookup(&vfs, "f").await;
        lookup(&vfs, "d").await;
        for path in ["f/..", "f/.", "f/x"] {
            let err = vfs
                .walk_path(root, path, O_RDONLY, Caller::NONE)
                .await
                .unwrap_err();
            assert_eq!(err.errno, ENOTDIR, "{path}");
        }

        let link = WalkResult {
            nodeid: NodeId::new(99),
            node_mode: MODE_SYMLINK,
            parent_nodeid: Some(NodeId::ROOT),
            rel_path: vec!["link".into()],
            last_to_be_created: false,
        };
        let err = vfs.walk_link(link, "f/..", Caller::NONE).await.unwrap_err();
        assert_eq!(err.errno, ENOTDIR);
        let up = vfs
            .walk_link(
                WalkResult {
                    nodeid: NodeId::new(99),
                    node_mode: MODE_SYMLINK,
                    parent_nodeid: Some(NodeId::ROOT),
                    rel_path: vec!["link".into()],
                    last_to_be_created: false,
                },
                "d/..",
                Caller::NONE,
            )
            .await
            .unwrap();
        assert_eq!(up.nodeid, NodeId::ROOT);
    });
    assert_eq!(mock.ops(), vec![u32::from(fuse_opcode::FUSE_LOOKUP); 2]);
}

#[test]
fn lookup_of_a_second_name_does_not_retarget_parent() {
    let (vfs, _) = vfs_with([file(10, 1), dir(11, 1), file(10, 1)]);
    block_on(async {
        let id = lookup(&vfs, "f").await;
        assert_eq!(vfs.state.borrow().inodes[&id].parent, NodeId::ROOT);
        lookup(&vfs, "d").await;
        vfs.lookup(NodeId::new(11), "g", Caller::NONE)
            .await
            .unwrap();
        assert_eq!(vfs.state.borrow().inodes[&id].parent, NodeId::ROOT);
        assert_eq!(vfs.state.borrow().dcache[&NodeId::new(11)]["g"].nodeid, id);
    });
}

#[test]
fn directory_relookup_updates_parent() {
    let (vfs, _) = vfs_with([dir(11, 1), dir(11, 1)]);
    block_on(async {
        let id = lookup(&vfs, "d").await;
        {
            let mut g = vfs.state.borrow_mut();
            g.inodes.get_mut(&id).unwrap().parent = id;
            g.dcache
                .get_mut(&NodeId::ROOT)
                .unwrap()
                .get_mut("d")
                .unwrap()
                .entry_expiry = Instant::now() - Duration::from_secs(1);
        }
        lookup(&vfs, "d").await;
        assert_eq!(vfs.state.borrow().inodes[&id].parent, NodeId::ROOT);
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
                vfs.rm(file_entry(&vfs, id, "victim"), false, Caller::NONE)
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
                        .lookup(NodeId::ROOT, &format!("f{i}"), Caller::NONE)
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
            .mkdir(new_entry(&vfs, "d"), MODE_DIR as u32, Caller::NONE)
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
            .open(
                NodeId::new(20),
                O_RDWR,
                false,
                vec!["src".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        let dest = vfs.entry_at(fd, "/dst", Caller::NONE).await.unwrap();
        let err = vfs
            .link(fd, dest, Caller::NONE)
            .await
            .expect_err("link generation mismatch");
        assert_eq!(err.errno, ESTALE);
    });
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

#[test]
fn rename_moves_the_name() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        bytes(&open_out(1)),
        MockReply::Host(ENOENT),
        MockReply::Unit,
    ]);
    block_on(async {
        lookup(&vfs, "a").await;
        let fd = vfs
            .open(
                NodeId::new(10),
                O_RDWR,
                false,
                vec!["a".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        rename_fd(&vfs, fd, "/b").await.unwrap();
    });
    assert_eq!(children(&vfs, NodeId::ROOT), 1);
    let g = vfs.state.borrow();
    assert!(g.dcache[&NodeId::ROOT].get("a").is_none());
    assert_eq!(g.dcache[&NodeId::ROOT]["b"].nodeid, NodeId::new(10));
    drop(g);
    assert_lru_consistent(&vfs);
    mock.assert_sent(fuse_opcode::FUSE_RENAME);
}

#[test]
fn rename_over_existing_name_drops_the_target() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        file(11, 1),
        bytes(&open_out(1)),
        MockReply::Unit, // FUSE_RENAME, with "b" resolved from the cache
    ]);
    block_on(async {
        lookup(&vfs, "a").await;
        lookup(&vfs, "b").await;
        let fd = vfs
            .open(
                NodeId::new(10),
                O_RDWR,
                false,
                vec!["a".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        rename_fd(&vfs, fd, "/b").await.unwrap();
    });
    assert!(!has_inode(&vfs, 11), "the unused target goes at once");
    assert!(!forget_queued(&vfs, 11));
    assert_eq!(children(&vfs, NodeId::ROOT), 1);
    assert_eq!(
        vfs.state.borrow().dcache[&NodeId::ROOT]["b"].nodeid,
        NodeId::new(10)
    );
    assert_lru_consistent(&vfs);
    assert_eq!(
        mock.ops(),
        [
            u32::from(fuse_opcode::FUSE_LOOKUP),
            u32::from(fuse_opcode::FUSE_LOOKUP),
            u32::from(fuse_opcode::FUSE_OPEN),
            u32::from(fuse_opcode::FUSE_RENAME),
            u32::from(fuse_opcode::FUSE_FORGET),
        ]
    );
}

#[test]
fn rename_over_stale_name_replaces_it() {
    let mut stale = entry(11, 1, MODE_FILE as u32);
    stale.entry_valid = 0;
    let (vfs, _) = vfs_with([
        file(10, 1),
        bytes(&stale),
        bytes(&open_out(1)),
        MockReply::Host(ENOENT), // the host no longer has "b"
        MockReply::Unit,         // FUSE_RENAME
    ]);
    block_on(async {
        lookup(&vfs, "a").await;
        lookup(&vfs, "b").await;
        let fd = vfs
            .open(
                NodeId::new(10),
                O_RDWR,
                false,
                vec!["a".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        rename_fd(&vfs, fd, "/b").await.unwrap();
    });
    let g = vfs.state.borrow();
    assert_eq!(g.dcache[&NodeId::ROOT].len(), 1);
    assert_eq!(g.dcache[&NodeId::ROOT]["b"].nodeid, NodeId::new(10));
    assert!(
        !g.dentry_index.contains_key(&NodeId::new(11)),
        "the stale name must not stay indexed"
    );
    drop(g);
    assert_lru_consistent(&vfs);
}

/// The destination directory is the only evictable inode when a concurrent lookup needs room, so
/// only the rename's pin on it keeps it cached until the new name is inserted.
#[test]
fn rename_into_evicted_parent_does_not_orphan_dentries() {
    let (vfs, mock) = vfs_with([
        file(10, 1),
        bytes(&open_out(1)),
        dir(11, 1),
        file(21, 1),
        file(22, 1),
        file(23, 1),
        file(24, 1),
        file(31, 1),
        MockReply::Host(ENOENT), // "b" does not exist in "d"
        file(32, 1),
        MockReply::Unit, // FUSE_RENAME
    ]);
    let fd = block_on(async {
        lookup(&vfs, "a").await;
        let fd = vfs
            .open(
                NodeId::new(10),
                O_RDWR,
                false,
                vec!["a".into()],
                Caller::NONE,
            )
            .await
            .unwrap();
        lookup(&vfs, "d").await;
        // Root, a, d and four open files fill all but one slot.
        for name in ["f21", "f22", "f23", "f24"] {
            let id = lookup(&vfs, name).await;
            vfs.state.borrow_mut().increment_fd_count(id).unwrap();
        }
        assert!(in_lru_evictables(&vfs, 11));
        fd
    });
    assert_eq!(vfs.state.borrow().lru_evictables.len(), 1);

    mock.yield_once.set(true);
    let vfs = Rc::new(vfs);

    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                for name in ["g1", "g2"] {
                    let _ = vfs.lookup(NodeId::ROOT, name, Caller::NONE).await;
                }
            }
        })
        .unwrap();
    spawner
        .spawn_local({
            let vfs = Rc::clone(&vfs);
            async move {
                rename_fd(&vfs, fd, "/d/b").await.unwrap();
            }
        })
        .unwrap();
    pool.run();

    assert!(
        has_inode(&vfs, 11),
        "the rename target directory was evicted"
    );
    // Proves the interleaving happened: inserting g2 needed room and g1 was the only victim.
    assert_eq!(vfs.state.borrow().evicted_names, ["g1"]);
    assert_eq!(children(&vfs, NodeId::new(11)), 1);
    assert_eq!(
        vfs.state.borrow().dcache[&NodeId::new(11)]["b"].nodeid,
        NodeId::new(10)
    );
    assert_lru_consistent(&vfs);
    mock.assert_sent(fuse_opcode::FUSE_RENAME);
}

#[test]
fn unmerge_of_busy_inode_drops_it_on_close() {
    let (vfs, _) = vfs_with([file(10, 1)]);
    block_on(async {
        let id = lookup(&vfs, "f").await;
        let mut g = vfs.state.borrow_mut();
        g.increment_fd_count(id).unwrap();
        g.unmerge_entry(NodeId::ROOT, "f", id);
        assert!(g.inodes.contains_key(&id), "an open inode must stay cached");
        assert!(g.inodes[&id].forget_pending);
        assert_eq!(
            g.forget_queue.iter().copied().collect::<Vec<_>>(),
            [(id, 1)]
        );

        g.decrement_fd_count(id);
        assert!(!g.inodes.contains_key(&id), "the last close drops it");
        assert_eq!(g.forget_queue.len(), 1, "the lookup is forgotten once");
        drop(g);
        assert_lru_consistent(&vfs);
    });
}

#[test]
fn unlink_is_not_logged_as_eviction() {
    let (vfs, _) = vfs_with([file(10, 1), MockReply::Unit]);
    block_on(async {
        let id = lookup(&vfs, "f").await;
        vfs.rm(file_entry(&vfs, id, "f"), false, Caller::NONE)
            .await
            .unwrap();
    });
    assert!(!has_inode(&vfs, 10));
    assert!(vfs.state.borrow().evicted_names.is_empty());
}

#[test]
fn symlink_rollback_drops_committed_dentry() {
    let (vfs, mock) = vfs_with([file(30, 1), MockReply::Unit]);
    mock.yield_once.set(true);
    let fd = vfs
        .pre_create_symlink(new_entry(&vfs, "link"), Caller::NONE)
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
                vfs.close(fd).await;
            }
        })
        .unwrap();
    pool.run();

    assert!(!has_inode(&vfs, 30));
    assert_eq!(children(&vfs, NodeId::ROOT), 0);
    assert_eq!(
        open_fds(&vfs, NodeId::ROOT),
        0,
        "close releases the parent once; the failed commit must not release it again"
    );
    assert_lru_consistent(&vfs);
    mock.assert_sent(fuse_opcode::FUSE_UNLINK);
}

/// Creating a symlink takes two calls. The parent must stay cached in between.
#[test]
fn symlink_pre_create_holds_the_parent_until_the_write() {
    let (vfs, _) = vfs_with([dir(11, 1), MockReply::Host(ENOENT), file(30, 1)]);
    let root = vfs.scheme_root();
    block_on(async {
        lookup(&vfs, "d").await;
        let entry = vfs.entry_at(root, "d/link", Caller::NONE).await.unwrap();
        let fd = vfs.pre_create_symlink(entry, Caller::NONE).unwrap();
        assert_eq!(open_fds(&vfs, NodeId::new(11)), 1);
        assert!(
            !in_lru_evictables(&vfs, 11),
            "pending link's parent was evictable"
        );

        vfs.write(fd, b"target", 0, O_RDWR as u32).await.unwrap();
        assert_eq!(
            open_fds(&vfs, NodeId::new(11)),
            0,
            "commit releases the parent"
        );
        assert_eq!(
            open_fds(&vfs, NodeId::new(30)),
            1,
            "the handle holds the link"
        );

        vfs.close(fd).await;
        assert_eq!(open_fds(&vfs, NodeId::new(30)), 0);
    });
    assert_lru_consistent(&vfs);
}

#[test]
fn symlink_pre_create_close_releases_the_parent() {
    let (vfs, mock) = vfs_with([dir(11, 1), MockReply::Host(ENOENT)]);
    let root = vfs.scheme_root();
    block_on(async {
        lookup(&vfs, "d").await;
        let entry = vfs.entry_at(root, "d/link", Caller::NONE).await.unwrap();
        let fd = vfs.pre_create_symlink(entry, Caller::NONE).unwrap();
        vfs.close(fd).await;
    });
    assert_eq!(open_fds(&vfs, NodeId::new(11)), 0);
    assert!(in_lru_evictables(&vfs, 11));
    assert_lru_consistent(&vfs);
    assert!(!mock.ops().contains(&u32::from(fuse_opcode::FUSE_SYMLINK)));
}

#[test]
fn creating_an_existing_name_is_eexist() {
    let (vfs, mock) = vfs_with([file(10, 1), dir(11, 1)]);
    block_on(async {
        let f = lookup(&vfs, "f").await;
        let d = lookup(&vfs, "d").await;
        let err = vfs
            .create(
                file_entry(&vfs, f, "f"),
                O_CREAT | O_RDWR,
                MODE_FILE as u32,
                Caller::NONE,
            )
            .await
            .expect_err("create over an existing name");
        assert_eq!(err.errno, EEXIST);
        let err = vfs
            .mkdir(file_entry(&vfs, d, "d"), MODE_DIR as u32, Caller::NONE)
            .await
            .expect_err("mkdir over an existing name");
        assert_eq!(err.errno, EEXIST);
        let err = vfs
            .pre_create_symlink(file_entry(&vfs, f, "f"), Caller::NONE)
            .expect_err("symlink over an existing name");
        assert_eq!(err.errno, EEXIST);
    });
    assert_eq!(mock.ops().len(), 2, "only the two lookups reach the host");
    assert_eq!(open_fds(&vfs, NodeId::ROOT), 0);
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
        vfs.open(
            NodeId::new(10),
            O_RDONLY,
            false,
            vec!["f".into()],
            Caller::NONE,
        )
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
            .open(NodeId::new(10), O_RDWR, false, vec!["f".into()], OPENER)
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
        async |vfs, fd, _| vfs.close(fd).await,
    );
    assert_eq!(uid, OPENER.uid);
}

#[test]
fn close_release_carries_the_opener() {
    let uid = uid_on_open_handle(
        MockReply::Unit,
        fuse_opcode::FUSE_RELEASE,
        async |vfs, fd, _| vfs.close(fd).await,
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

#[test]
fn fuse_open_omits_creat_excl() {
    let flags = O_RDWR | O_CREAT | O_EXCL;
    let open = fuse_open_flags(flags, false, false).unwrap();
    assert_eq!(open & (linux_abi::O_CREAT | linux_abi::O_EXCL), 0);
    let create = fuse_open_flags(flags, false, true).unwrap();
    assert_eq!(
        create & (linux_abi::O_CREAT | linux_abi::O_EXCL),
        linux_abi::O_CREAT | linux_abi::O_EXCL
    );
}

fn read_debug_log(vfs: &Vfs<Mock>, fd: usize) -> String {
    let mut buf = [0u8; 256];
    let n = block_on(vfs.read(fd, &mut buf, 0, O_RDONLY as u32)).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

#[test]
fn debug_root_only_serves_lru_evicted() {
    let (vfs, mock) = vfs_with([]);
    let root = vfs.debug_root();
    assert!(vfs.is_debug(root));

    let fd = vfs.open_debug(root, "lru_evicted", O_RDONLY).unwrap();
    assert!(vfs.is_debug(fd));
    assert_eq!(read_debug_log(&vfs, fd), "");

    for path in ["", "/", ".lru_evicted", "fixtures", "lru_evicted/x"] {
        let err = vfs.open_debug(root, path, O_RDONLY).unwrap_err();
        assert_eq!(err.errno, ENOENT, "{path:?}");
    }
    let err = vfs.open_debug(fd, "lru_evicted", O_RDONLY).unwrap_err();
    assert_eq!(err.errno, ENOTDIR);
    assert!(mock.ops().is_empty());
}

#[test]
fn share_root_does_not_expose_debug_nodes() {
    let (vfs, mock) = vfs_with([MockReply::Host(ENOENT), MockReply::Host(ENOENT)]);
    let root = vfs.scheme_root();
    assert!(!vfs.is_debug(root));

    for name in ["lru_evicted", ".lru_evicted"] {
        let err = block_on(vfs.walk_path(root, name, O_RDONLY, Caller::NONE))
            .err()
            .unwrap();
        assert_eq!(err.errno, ENOENT);
    }
    assert_eq!(mock.ops(), vec![u32::from(fuse_opcode::FUSE_LOOKUP); 2]);
}

#[test]
fn evictions_are_logged_and_truncate_clears_them() {
    // One more lookup than fits, so the least recently used name is evicted.
    let (vfs, _mock) = vfs_with((2..=9).map(|i| file(i, 1)));
    for i in 2..=9 {
        block_on(vfs.lookup(NodeId::ROOT, &format!("f{i}"), Caller::NONE)).unwrap();
    }

    let root = vfs.debug_root();
    let fd = vfs.open_debug(root, "lru_evicted", O_RDONLY).unwrap();
    let log = read_debug_log(&vfs, fd);
    assert_eq!(log.lines().next(), Some("f2"), "{log:?}");

    let cleared = vfs
        .open_debug(root, "lru_evicted", O_RDONLY | O_TRUNC)
        .unwrap();
    assert_eq!(read_debug_log(&vfs, cleared), "");
}

#[test]
fn debug_handles_cannot_walk_the_share() {
    let (vfs, mock) = vfs_with([]);
    let root = vfs.debug_root();
    let log = vfs.open_debug(root, "lru_evicted", O_RDONLY).unwrap();

    // Absolute paths resolve from the share root for ordinary handles. They
    // must not do that for debug handles (this is how `unlinkat` gets here).
    for fd in [root, log] {
        for path in ["/", "/file", "file", "../file"] {
            let err = block_on(vfs.walk_path(fd, path, O_RDONLY, Caller::NONE))
                .err()
                .unwrap();
            assert_eq!(err.errno, EPERM, "fd {fd} path {path:?}");
        }
    }
    assert!(mock.ops().is_empty());
}
