use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::time::{Duration, Instant};

use fuse_abi::linux::{
    FATTR_ATIME, FATTR_ATIME_NOW, FATTR_FH, FATTR_GID, FATTR_MODE, FATTR_MTIME, FATTR_MTIME_NOW,
    FATTR_SIZE, FATTR_UID, FUSE_ATOMIC_O_TRUNC, fuse_setattr_in,
};
use log::warn;
use scheme_utils::HandleMap;
use syscall::error::{ESTALE, Error, Result};
use syscall::{
    EBADF, EINVAL, EISDIR, ELOOP, ENOENT, ENOTDIR, MODE_DIR, MODE_SYMLINK, MODE_TYPE, O_ACCMODE,
    O_APPEND, O_CREAT, O_DIRECTORY, O_EXCL, O_NONBLOCK, O_RDWR, O_TRUNC, O_WRONLY, TimeSpec,
};

use crate::fuse::{
    Caller, FUSE_ROOT_ID, FuseReaddirOut, FuseSession, FuseTransporter, fuse_attr, fuse_attr_out,
    fuse_entry_out,
};
use crate::linux_abi;

// futimens puts these in TimeSpec.tv_nsec:
// - UTIME_NOW = "use the current time",
// - UTIME_OMIT = "leave this stamp alone".
// They are not nanoseconds (NOW is > 1e9), so they cannot go on the wire as atimensec/mtimensec.
// FUSE uses FATTR_ATIME_NOW / FATTR_MTIME_NOW for NOW, and no bit at all for OMIT.
// These are used by `Vfs::apply_timespec`.
const UTIME_NOW: i32 = 0x3fffffff;
const UTIME_OMIT: i32 = 0x3ffffffe;

const INODES_MAX_LEN: usize = 64 * 1024;
const SYMLOOP_MAX: u32 = 40;
const FORGET_QUEUE_MIN_CAP: usize = 1024;

#[cfg(feature = "lru-debug")]
const EVICT_LOG_CAP: usize = 8192;

#[cfg(feature = "lru-debug")]
/// Debug node at the scheme root: one evicted dcache name per line, LRU order.
pub const LRU_EVICTED_NAME: &str = ".lru_evicted";

pub struct WalkResult {
    pub nodeid: u64,
    pub node_mode: u16,
    parent_nodeid: u64,
    pub rel_path: Vec<String>,
    pub last_to_be_created: bool,
}

struct Inode {
    nodeid: u64,
    generation: Option<u64>,
    parent: u64,
    attr: fuse_attr,
    attr_expiry: Instant,
    nlookup: u64,
    forget_pending: bool,
    open_fd_count: u64,
    child_count: u64,
    used: u64,
}

impl Inode {
    fn attr_valid(&self) -> bool {
        Instant::now() < self.attr_expiry
    }

    fn mode_type(&self) -> u16 {
        self.attr.mode as u16 & MODE_TYPE
    }
}

struct Dentry {
    nodeid: u64,
    entry_expiry: Instant,
}

impl Dentry {
    fn is_valid(&self) -> bool {
        Instant::now() < self.entry_expiry
    }
}

enum Handle {
    File {
        nodeid: u64,
        fh: u64,
        path: String,
    },
    Directory {
        nodeid: u64,
        fh: u64,
        path: String,
    },
    StatOnly {
        nodeid: u64,
        path: String,
    },
    Symlink {
        nodeid: u64,
        path: String,
        target: String,
    },
    /// Redox doesn't have a dedicated symlink creation scheme call,
    /// relibc splits that into a create plus a write.
    SymlinkPreCreate {
        parent_nodeid: u64,
        name: String,
        path: String,
    },
    SchemeRoot,
    #[cfg(feature = "lru-debug")]
    LruLog,
}

pub struct Vfs<T: FuseTransporter> {
    fuse: FuseSession<T>,
    inodes: BTreeMap<u64, Inode>,
    dcache: BTreeMap<u64, BTreeMap<String, Dentry>>,
    handles: HandleMap<Handle>,
    forget_queue: VecDeque<(u64, u64)>,
    next_used: u64,
    #[cfg(feature = "lru-debug")]
    evicted_names: Vec<String>,
}

impl<T: FuseTransporter> Vfs<T> {
    // Private API

    fn touch(&mut self, nodeid: u64) {
        self.next_used += 1;
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.used = self.next_used;
        }
    }

    fn merge_entry(&mut self, parent: u64, name: &str, entry: fuse_entry_out) -> Result<()> {
        let now = Instant::now();
        let attr_expiry = fuse_expiry(now, entry.attr_valid, entry.attr_valid_nsec);
        let entry_expiry = fuse_expiry(now, entry.entry_valid, entry.entry_valid_nsec);

        let old_parent = if let Some(inode) = self.inodes.get_mut(&entry.nodeid) {
            match inode.generation {
                None => inode.generation = Some(entry.generation),
                Some(g) if g != entry.generation => {
                    warn!(
                        "virtio-fsd: generation mismatch nodeid={} have={g} got={}",
                        entry.nodeid, entry.generation
                    );
                    return Err(Error::new(ESTALE));
                }
                Some(_) => {}
            }
            inode.nlookup += 1;
            inode.attr = entry.attr;
            inode.attr_expiry = attr_expiry;
            let old_parent = inode.parent;
            inode.parent = parent;
            Some(old_parent)
        } else {
            self.populate_forgets();
            self.inodes.insert(
                entry.nodeid,
                Inode {
                    nodeid: entry.nodeid,
                    generation: Some(entry.generation),
                    parent,
                    attr: entry.attr,
                    attr_expiry,
                    nlookup: 1,
                    forget_pending: false,
                    open_fd_count: 0,
                    child_count: 0,
                    used: 0,
                },
            );
            if let Some(p) = self.inodes.get_mut(&parent) {
                p.child_count += 1;
            }
            None
        };

        if let Some(old_parent) = old_parent
            && old_parent != parent
        {
            if let Some(p) = self.inodes.get_mut(&old_parent) {
                p.child_count = p.child_count.saturating_sub(1);
            }
            if let Some(p) = self.inodes.get_mut(&parent) {
                p.child_count += 1;
            }
        }

        self.dcache.entry(parent).or_default().insert(
            name.to_owned(),
            Dentry {
                nodeid: entry.nodeid,
                entry_expiry,
            },
        );
        self.touch(entry.nodeid);
        Ok(())
    }

    fn increment_fd_count(&mut self, nodeid: u64) -> Result<()> {
        let inode = self.inodes.get_mut(&nodeid).ok_or_else(|| {
            warn!("virtio-fsd: increment_fd_count: no inode for nodeid={nodeid}");
            Error::new(EBADF)
        })?;
        inode.open_fd_count += 1;
        Ok(())
    }

    fn decrement_fd_count(&mut self, nodeid: u64) {
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            if inode.open_fd_count > 0 {
                inode.open_fd_count -= 1;
            }

            if inode.forget_pending && inode.open_fd_count == 0 {
                if inode.nlookup > 0 {
                    self.forget_queue.push_back((nodeid, inode.nlookup));
                }
                self.inodes.remove(&nodeid);
            }
        } else {
            warn!("virtio-fsd: decrement_fd_count: no inode for nodeid={nodeid}");
        }
    }

    fn cached_child(&self, parent: u64, name: &str) -> Option<u64> {
        let dentry = self.dcache.get(&parent)?.get(name)?;
        if !dentry.is_valid() || !self.inodes.contains_key(&dentry.nodeid) {
            return None;
        }
        Some(dentry.nodeid)
    }

    #[cfg(feature = "lru-debug")]
    fn dcache_name(&self, nodeid: u64) -> Option<String> {
        for names in self.dcache.values() {
            for (name, d) in names {
                if d.nodeid == nodeid {
                    return Some(name.clone());
                }
            }
        }
        None
    }

    fn drop_dentry(&mut self, parent: u64, name: &str) {
        let Some(map) = self.dcache.get_mut(&parent) else {
            return;
        };
        map.remove(name);
        if map.is_empty() {
            self.dcache.remove(&parent);
        }
    }

    fn refresh_attr(&mut self, nodeid: u64, payload: fuse_attr_out) -> u16 {
        let mode = payload.attr.mode as u16 & MODE_TYPE;
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.attr = payload.attr;
            inode.attr_expiry =
                fuse_expiry(Instant::now(), payload.attr_valid, payload.attr_valid_nsec);
        }
        mode
    }

    fn pick_lru_victim(&self) -> Option<u64> {
        self.inodes
            .iter()
            .filter(|(id, i)| **id != FUSE_ROOT_ID && i.open_fd_count == 0 && i.child_count == 0)
            .min_by_key(|(_, i)| i.used)
            .map(|(id, _)| *id)
    }

    fn populate_forgets(&mut self) {
        while self.inodes.len() >= INODES_MAX_LEN {
            let Some(id) = self.pick_lru_victim() else {
                warn!(
                    "virtio-fsd: LRU full ({}/{}), nothing evictable",
                    self.inodes.len(),
                    INODES_MAX_LEN
                );
                break;
            };
            let Some(inode) = self.inodes.remove(&id) else {
                break;
            };
            #[cfg(feature = "lru-debug")]
            {
                let name = self.dcache_name(id);
                if let Some(name) = name {
                    if self.evicted_names.len() >= EVICT_LOG_CAP {
                        self.evicted_names.remove(0);
                    }
                    self.evicted_names.push(name);
                }
            }
            if let Some(parent) = self.inodes.get_mut(&inode.parent) {
                parent.child_count = parent.child_count.saturating_sub(1);
            }

            self.dcache.remove(&id);
            self.dcache.retain(|_, names| {
                names.retain(|_, d| d.nodeid != id);
                !names.is_empty()
            });

            if inode.nlookup > 0 {
                self.forget_queue.push_back((id, inode.nlookup));
            }
        }
    }

    fn inode_mode(&self, nodeid: u64) -> Result<u16> {
        self.inodes
            .get(&nodeid)
            .map(Inode::mode_type)
            .ok_or(Error::new(ESTALE))
    }

    fn invalidate_attr(&mut self, nodeid: u64) {
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.attr_expiry = Instant::now();
        }
    }

    async fn flush_forgets(&mut self, caller: Caller) {
        while let Some((nodeid, nlookup)) = self.forget_queue.pop_front() {
            if let Err(err) = self.fuse.forget(nodeid, nlookup, caller).await {
                warn!("virtio-fsd: FUSE_FORGET: {err}");
                self.forget_queue.push_front((nodeid, nlookup));
                break;
            }
        }
    }

    // Public API

    pub fn new(fuse_session: FuseSession<T>) -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            FUSE_ROOT_ID,
            Inode {
                nodeid: FUSE_ROOT_ID,
                generation: None,
                parent: FUSE_ROOT_ID,
                attr: fuse_attr {
                    ino: FUSE_ROOT_ID,
                    mode: MODE_DIR as u32,
                    ..fuse_attr::default()
                },
                attr_expiry: Instant::now(),
                nlookup: 0,
                forget_pending: false,
                open_fd_count: 0,
                child_count: 0,
                used: 0,
            },
        );

        Self {
            fuse: fuse_session,
            inodes,
            dcache: BTreeMap::new(),
            handles: HandleMap::new(),
            forget_queue: VecDeque::with_capacity(FORGET_QUEUE_MIN_CAP),
            next_used: 0,
            #[cfg(feature = "lru-debug")]
            evicted_names: Vec::new(),
        }
    }

    pub async fn lookup(&mut self, parent: u64, name: &str, caller: Caller) -> Result<(u64, u16)> {
        if name.is_empty() {
            return Err(Error::new(ENOENT));
        }

        if let Some(nodeid) = self.cached_child(parent, name) {
            if let Some(inode) = self.inodes.get(&nodeid)
                && inode.attr_valid()
            {
                let mode = inode.mode_type();
                self.touch(nodeid);
                return Ok((nodeid, mode));
            }

            let payload = match self.fuse.getattr(nodeid, None, caller).await {
                Ok(payload) => payload,
                Err(e) => {
                    let err = e.into_syscall("FUSE_GETATTR");
                    if err.errno == ENOENT {
                        self.drop_dentry(parent, name);
                    }
                    return Err(err);
                }
            };

            let mode = self.refresh_attr(nodeid, payload);
            self.touch(nodeid);
            return Ok((nodeid, mode));
        }

        let name_cstring = CString::new(name).map_err(|_| {
            warn!("virtio-fsd: lookup: dir name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let payload = self
            .fuse
            .lookup(parent, name_cstring, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_LOOKUP"))?;
        let nodeid = payload.nodeid;
        let mode = payload.attr.mode as u16 & MODE_TYPE;
        self.merge_entry(parent, name, payload)?;
        self.flush_forgets(caller).await;
        Ok((nodeid, mode))
    }

    pub async fn mkdir(
        &mut self,
        parent: u64,
        name: &str,
        mode: u32,
        caller: Caller,
    ) -> Result<(u64, u16)> {
        if name.is_empty() {
            return Err(Error::new(ENOENT));
        }

        let name_cstring = CString::new(name).map_err(|_| {
            warn!("virtio-fsd: mkdir: dir name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let payload = self
            .fuse
            .mkdir(parent, name_cstring.clone(), mode, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_MKDIR"))?;

        self.invalidate_attr(parent);

        let nodeid = payload.nodeid;
        let mode = payload.attr.mode as u16 & MODE_TYPE;

        if let Err(err) = self.merge_entry(parent, name, payload) {
            if let Err(rm_err) = self.fuse.rm(parent, name_cstring, true, caller).await {
                warn!("virtio-fsd: FUSE_RMDIR failed after a merge_entry fail: {rm_err}");
            }
            return Err(err);
        }

        self.flush_forgets(caller).await;
        Ok((nodeid, mode))
    }

    pub async fn create(
        &mut self,
        parent_nodeid: u64,
        name: &str,
        flags: usize,
        mode: u32,
        path: Vec<String>,
        caller: Caller,
    ) -> Result<usize> {
        if name.is_empty() {
            return Err(Error::new(ENOENT));
        }

        let fuse_flags = fuse_open_flags(flags, false, true)?;

        let name_cstring = CString::new(name).map_err(|_| {
            warn!("virtio-fsd: create: file name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let payload = self
            .fuse
            .create(parent_nodeid, name_cstring, fuse_flags, mode, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_CREATE"))?;

        self.invalidate_attr(parent_nodeid);

        let nodeid = payload.entry.nodeid;

        if let Err(err) = self.merge_entry(parent_nodeid, name, payload.entry) {
            if let Err(rel_err) = self
                .fuse
                .release(nodeid, payload.open.fh, false, caller)
                .await
            {
                warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
            }
            return Err(err);
        }

        if let Err(err) = self.increment_fd_count(nodeid) {
            if let Err(rel_err) = self
                .fuse
                .release(nodeid, payload.open.fh, false, caller)
                .await
            {
                warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
            }
            return Err(err);
        }

        self.flush_forgets(caller).await;

        Ok(self.handles.insert(Handle::File {
            nodeid,
            fh: payload.open.fh,
            path: path.join("/"),
        }))
    }

    pub async fn open(
        &mut self,
        nodeid: u64,
        flags: usize,
        is_dir: bool,
        path: Vec<String>,
        caller: Caller,
    ) -> Result<usize> {
        let opcode = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };
        let mut open_flags = fuse_open_flags(flags, is_dir, false)?;
        let trunc = !is_dir && open_flags & linux_abi::O_TRUNC != 0;
        let atomic_trunc = self.fuse.capable(FUSE_ATOMIC_O_TRUNC);
        if trunc && !atomic_trunc {
            open_flags &= !linux_abi::O_TRUNC;
        }

        let payload = self
            .fuse
            .open(nodeid, open_flags, is_dir, caller)
            .await
            .map_err(|e| e.into_syscall(opcode))?;

        let fh = payload.fh;

        if let Err(err) = self.increment_fd_count(nodeid) {
            if let Err(rel_err) = self.fuse.release(nodeid, fh, is_dir, caller).await {
                warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
            }
            return Err(err);
        }

        let handle = if is_dir {
            Handle::Directory {
                nodeid,
                fh,
                path: path.join("/"),
            }
        } else {
            Handle::File {
                nodeid,
                fh,
                path: path.join("/"),
            }
        };
        let fd = self.handles.insert(handle);

        if trunc && !atomic_trunc {
            if let Err(err) = self.ftruncate(fd, 0, caller).await {
                self.close(fd, caller).await;
                return Err(err);
            }
        } else if trunc {
            self.invalidate_attr(nodeid);
        }

        Ok(fd)
    }

    pub async fn getattr(&mut self, fd: usize, caller: Caller) -> Result<fuse_attr> {
        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => (*nodeid, None),
            Handle::SchemeRoot => (FUSE_ROOT_ID, None),
            Handle::SymlinkPreCreate { .. } => return Err(Error::new(EBADF)),
            #[cfg(feature = "lru-debug")]
            Handle::LruLog => {
                return Ok(fuse_attr {
                    ino: 0,
                    size: self.evicted_names.join("\n").len() as u64,
                    mode: 0o100000,
                    ..fuse_attr::default()
                });
            }
        };

        if let Some(inode) = self.inodes.get(&nodeid)
            && inode.attr_valid()
        {
            let attr = inode.attr;
            self.touch(nodeid);
            return Ok(attr);
        }

        let payload = self
            .fuse
            .getattr(nodeid, fh, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_GETATTR"))?;

        self.refresh_attr(nodeid, payload);
        self.touch(nodeid);
        Ok(payload.attr)
    }

    pub async fn read(
        &mut self,
        fd: usize,
        buf: &mut [u8],
        offset: u64,
        caller: Caller,
    ) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => self
                .fuse
                .read(*nodeid, *fh, offset, buf, 0, 0, caller)
                .await
                .map_err(|e| e.into_syscall("FUSE_READ")),
            Handle::Symlink { target, .. } => {
                let data = target.as_bytes();
                let off = offset as usize;
                if off >= data.len() {
                    return Ok(0);
                }
                let n = (data.len() - off).min(buf.len());
                buf[..n].copy_from_slice(&data[off..off + n]);
                Ok(n)
            }
            #[cfg(feature = "lru-debug")]
            Handle::LruLog => {
                let data = self.evicted_names.join("\n");
                let off = offset as usize;
                if off >= data.len() {
                    return Ok(0);
                }
                let n = (data.len() - off).min(buf.len());
                buf[..n].copy_from_slice(&data.as_bytes()[off..off + n]);
                Ok(n)
            }
            Handle::Directory { .. } => Err(Error::new(EISDIR)),
            _ => {
                warn!("virtio-fsd: read: fd '{fd}' is not associated with a valid file.");
                Err(Error::new(EBADF))
            }
        }
    }

    pub async fn write(
        &mut self,
        fd: usize,
        buf: &[u8],
        offset: u64,
        caller: Caller,
    ) -> Result<usize> {
        let (parent_nodeid, name, path) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => {
                if buf.is_empty() {
                    return Ok(0);
                }
                let nodeid = *nodeid;
                let fh = *fh;
                let payload = self
                    .fuse
                    .write(nodeid, fh, offset, buf, 0, 0, caller)
                    .await
                    .map_err(|e| e.into_syscall("FUSE_WRITE"))?;
                self.invalidate_attr(nodeid);
                return Ok(payload.size as usize);
            }
            Handle::SymlinkPreCreate {
                parent_nodeid,
                name,
                path,
            } => (*parent_nodeid, name.clone(), path.clone()),
            Handle::Directory { .. } => return Err(Error::new(EISDIR)),
            Handle::Symlink { .. } | Handle::StatOnly { .. } | Handle::SchemeRoot => {
                return Err(Error::new(EBADF));
            }
            #[cfg(feature = "lru-debug")]
            Handle::LruLog => return Err(Error::new(EBADF)),
        };

        if offset != 0 {
            return Err(Error::new(EINVAL));
        }

        let name_cstring = CString::new(name.as_str()).map_err(|_| {
            warn!("virtio-fsd: write symlink: file name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let target = std::str::from_utf8(buf)
            .map_err(|e| {
                warn!("virtio-fsd: write symlink: target is not valid utf8: {e:?}");
                Error::new(EINVAL)
            })?
            .to_owned();
        let target_cstring = CString::new(target.as_str()).map_err(|_| {
            warn!("virtio-fsd: write symlink: target contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let payload = self
            .fuse
            .symlink(parent_nodeid, &name_cstring, &target_cstring, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_SYMLINK"))?;

        let nodeid = payload.nodeid;
        if let Err(err) = self.merge_entry(parent_nodeid, &name, payload) {
            if let Err(rm_err) = self
                .fuse
                .rm(parent_nodeid, name_cstring, false, caller)
                .await
            {
                warn!("virtio-fsd: FUSE_UNLINK failed after a merge_entry fail: {rm_err}");
            }
            return Err(err);
        }
        self.flush_forgets(caller).await;

        self.invalidate_attr(parent_nodeid);
        self.increment_fd_count(nodeid)?;

        *self.handles.get_mut(fd)? = Handle::Symlink {
            nodeid,
            path,
            target,
        };
        Ok(buf.len())
    }

    pub async fn fsync(&self, fd: usize, caller: Caller) -> Result<()> {
        let (nodeid, fh, is_dir) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => (*nodeid, *fh, false),
            Handle::Directory { nodeid, fh, .. } => (*nodeid, *fh, true),
            _ => {
                warn!("virtio-fsd: fsync is called on an invalid fd: {fd}");
                return Err(Error::new(EBADF));
            }
        };

        let opcode = if is_dir {
            "FUSE_FSYNCDIR"
        } else {
            "FUSE_FSYNC"
        };

        self.fuse
            .fsync(nodeid, fh, is_dir, caller)
            .await
            .map_err(|e| e.into_syscall(opcode))
    }

    async fn setattr(
        &mut self,
        nodeid: u64,
        fuse_in: fuse_setattr_in,
        caller: Caller,
    ) -> Result<()> {
        let payload = self
            .fuse
            .setattr(nodeid, fuse_in, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_SETATTR"))?;
        self.refresh_attr(nodeid, payload);
        self.touch(nodeid);
        Ok(())
    }

    pub async fn ftruncate(&mut self, fd: usize, len: u64, caller: Caller) -> Result<()> {
        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => (*nodeid, Some(*fh)),
            Handle::Directory { .. } => return Err(Error::new(EISDIR)),
            _ => return Err(Error::new(EBADF)),
        };

        let mut fuse_in = fuse_setattr_in {
            size: len,
            ..Default::default()
        };
        fuse_in.valid = FATTR_SIZE;
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh;
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn futimens(&mut self, fd: usize, times: &[TimeSpec], caller: Caller) -> Result<()> {
        let atime = times.first().ok_or(Error::new(EINVAL))?;
        let mtime = times.get(1).ok_or(Error::new(EINVAL))?;

        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => (*nodeid, None),
            _ => return Err(Error::new(EBADF)),
        };

        let mut fuse_in = fuse_setattr_in::default();
        apply_timespec(&mut fuse_in, atime, true)?;
        apply_timespec(&mut fuse_in, mtime, false)?;
        if fuse_in.valid == 0 {
            return Ok(());
        }
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh;
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn fchmod(&mut self, fd: usize, new_mode: u16, caller: Caller) -> Result<()> {
        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => (*nodeid, None),
            _ => return Err(Error::new(EBADF)),
        };

        let type_bits = self
            .inodes
            .get(&nodeid)
            .map(|i| i.attr.mode & MODE_TYPE as u32)
            .unwrap_or(0);

        let mut fuse_in = fuse_setattr_in {
            mode: (new_mode as u32 & 0o7777) | type_bits,
            ..Default::default()
        };
        fuse_in.valid = FATTR_MODE;
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh;
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn fchown(&mut self, fd: usize, uid: u32, gid: u32, caller: Caller) -> Result<()> {
        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => (*nodeid, None),
            _ => return Err(Error::new(EBADF)),
        };

        let mut fuse_in = fuse_setattr_in::default();
        // POSIX: uid/gid -1 (here u32::MAX) means "leave this id alone".
        if uid != u32::MAX {
            fuse_in.valid |= FATTR_UID;
            fuse_in.uid = uid;
        }
        if gid != u32::MAX {
            fuse_in.valid |= FATTR_GID;
            fuse_in.gid = gid;
        }
        if fuse_in.valid == 0 {
            return Ok(());
        }
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh;
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn close(&mut self, fd: usize, caller: Caller) {
        match self.handles.remove(fd) {
            Some(handle) => match handle {
                Handle::File { nodeid, fh, .. } => {
                    if let Err(err) = self.fuse.release(nodeid, fh, false, caller).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.decrement_fd_count(nodeid);
                }
                Handle::Directory { nodeid, fh, .. } => {
                    if let Err(err) = self.fuse.release(nodeid, fh, true, caller).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.decrement_fd_count(nodeid);
                }
                Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => {
                    self.decrement_fd_count(nodeid);
                }
                Handle::SchemeRoot => {}
                Handle::SymlinkPreCreate { .. } => {}
                #[cfg(feature = "lru-debug")]
                Handle::LruLog => {}
            },
            None => {
                warn!("virtio-fsd: close: fd '{fd}' was not found in handles");
            }
        };

        self.flush_forgets(caller).await;
    }

    pub async fn shutdown(&mut self) {
        let ids: Vec<usize> = self.handles.keys().copied().collect();
        for id in ids {
            self.close(id, Caller::NONE).await;
        }

        for (nodeid, inode) in &self.inodes {
            if *nodeid != FUSE_ROOT_ID && inode.nlookup > 0 {
                self.forget_queue.push_back((*nodeid, inode.nlookup));
            }
        }
        self.flush_forgets(Caller::NONE).await;
    }

    pub fn scheme_root(&mut self) -> usize {
        self.handles.insert(Handle::SchemeRoot)
    }

    #[cfg(feature = "lru-debug")]
    pub fn open_lru_log(&mut self, clear: bool) -> usize {
        if clear {
            self.evicted_names.clear();
        }
        self.handles.insert(Handle::LruLog)
    }

    /// Open the synthetic `.lru_evicted` debug node, or leave the call to FUSE.
    ///
    /// That name is not a FUSE inode. It exists only at scheme root, so this
    /// matches a single path component from an absolute path, a scheme-root
    /// handle, or a directory whose nodeid is `FUSE_ROOT_ID`. Nested
    /// `.lru_evicted` (and every other path) returns `Ok(None)` and is walked
    /// normally. `O_TRUNC` clears the in-memory eviction log.
    #[cfg(feature = "lru-debug")]
    pub fn try_open_lru_log(
        &mut self,
        fd: usize,
        path: &str,
        flags: usize,
    ) -> Result<Option<usize>> {
        let parts: Vec<&str> = path_components(path);
        if parts.as_slice() != [LRU_EVICTED_NAME] {
            return Ok(None);
        }
        let at_root = if path.starts_with('/') {
            true
        } else {
            match self.handles.get(fd)? {
                Handle::SchemeRoot => true,
                Handle::Directory { nodeid, .. } | Handle::StatOnly { nodeid, .. } => {
                    *nodeid == FUSE_ROOT_ID
                }
                Handle::File { .. }
                | Handle::Symlink { .. }
                | Handle::SymlinkPreCreate { .. }
                | Handle::LruLog => false,
            }
        };
        if !at_root {
            return Ok(None);
        }
        let clear = flags & syscall::O_TRUNC != 0;
        Ok(Some(self.open_lru_log(clear)))
    }

    pub(crate) async fn follow_chain(
        &mut self,
        mut walk: WalkResult,
        caller: Caller,
    ) -> Result<WalkResult> {
        let mut hops = 0u32;
        while walk.node_mode == MODE_SYMLINK {
            hops += 1;
            if hops > SYMLOOP_MAX {
                return Err(Error::new(ELOOP));
            }
            let link_path = self.readlink(walk.nodeid, caller).await?;
            walk = self.walk_link(walk, &link_path, caller).await?;
        }
        Ok(walk)
    }

    pub async fn walk_path(
        &mut self,
        fd: usize,
        path: &str,
        open_flags: usize,
        caller: Caller,
    ) -> Result<WalkResult> {
        let mut walk = WalkResult {
            nodeid: 0,
            node_mode: 0,
            parent_nodeid: 0,
            rel_path: Vec::new(),
            last_to_be_created: false,
        };

        // TODO: revalidate path evaluation
        (walk.nodeid, walk.node_mode, walk.rel_path) = if path.starts_with('/') {
            (FUSE_ROOT_ID, MODE_DIR, Vec::new())
        } else {
            match self.handles.get(fd)? {
                Handle::File { .. } | Handle::Symlink { .. } | Handle::SymlinkPreCreate { .. } => {
                    Err(Error::new(ENOTDIR))
                }
                #[cfg(feature = "lru-debug")]
                Handle::LruLog => Err(Error::new(ENOTDIR)),
                Handle::Directory { nodeid, path, .. } => {
                    let mode = self.inode_mode(*nodeid)?;
                    Ok((*nodeid, mode, path_components(path)))
                }
                Handle::StatOnly { nodeid, path } => {
                    let mode = self.inode_mode(*nodeid)?;
                    if mode != MODE_DIR {
                        return Err(Error::new(ENOTDIR));
                    }
                    Ok((*nodeid, mode, path_components(path)))
                }
                Handle::SchemeRoot => Ok((FUSE_ROOT_ID, MODE_DIR, Vec::new())),
            }?
        };

        let path_parts: Vec<&str> = path_components(path);

        if !path.is_empty() && path != "/" {
            for (i, name) in path_parts.iter().copied().enumerate() {
                let is_last_element = i == path_parts.len() - 1;

                if name.is_empty() || name == "." {
                    continue;
                }

                if name == ".." {
                    (walk.nodeid, walk.node_mode) = self.parent_of(walk.nodeid)?;
                    walk.rel_path.pop();
                    continue;
                }

                walk.rel_path.push(name.to_owned());

                walk.parent_nodeid = walk.nodeid;
                match self.lookup(walk.parent_nodeid, name, caller).await {
                    Ok((id, mode)) => {
                        walk.nodeid = id;
                        walk.node_mode = mode;
                    }
                    Err(err) => {
                        if is_last_element
                            && err.errno == ENOENT
                            && (open_flags & syscall::O_CREAT != 0)
                        {
                            walk.last_to_be_created = true;
                            break;
                        }
                        return Err(err);
                    }
                }

                if !is_last_element && walk.node_mode == MODE_SYMLINK {
                    walk = self.follow_chain(walk, caller).await?;
                    if walk.node_mode != MODE_DIR {
                        return Err(Error::new(ENOTDIR));
                    }
                }
            }
        }
        Ok(walk)
    }

    pub async fn walk_link(
        &mut self,
        mut walk: WalkResult,
        path: &str,
        caller: Caller,
    ) -> Result<WalkResult> {
        if path.starts_with('/') {
            walk.nodeid = FUSE_ROOT_ID;
            walk.node_mode = MODE_DIR;
            walk.parent_nodeid = FUSE_ROOT_ID;
            walk.rel_path.clear();
        } else {
            // relative targets are relative to the directory that contains the link
            walk.rel_path.pop();
            if walk.parent_nodeid != 0 {
                walk.nodeid = walk.parent_nodeid;
                walk.node_mode = MODE_DIR;
            } else {
                (walk.nodeid, walk.node_mode) = self.parent_of(walk.nodeid)?;
            }
        }

        let path_parts: Vec<&str> = path_components(path);

        if !path.is_empty() && path != "/" {
            for name in path_parts.iter().copied() {
                if name.is_empty() || name == "." {
                    continue;
                }

                if name == ".." {
                    (walk.nodeid, walk.node_mode) = self.parent_of(walk.nodeid)?;
                    walk.rel_path.pop();
                    continue;
                }

                walk.rel_path.push(name.to_owned());

                walk.parent_nodeid = walk.nodeid;
                match self.lookup(walk.parent_nodeid, name, caller).await {
                    Ok((id, mode)) => {
                        walk.nodeid = id;
                        walk.node_mode = mode;
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(walk)
    }

    pub async fn readdir(
        &mut self,
        fd: usize,
        offset: u64,
        caller: Caller,
    ) -> Result<FuseReaddirOut> {
        match self.handles.get(fd)? {
            Handle::Directory { nodeid, fh, .. } => {
                let (nodeid, fh) = (*nodeid, *fh);
                self.fuse
                    .readdir(nodeid, fh, offset, caller)
                    .await
                    .map_err(|e| e.into_syscall("FUSE_READDIR"))
            }
            Handle::File { .. } => Err(Error::new(ENOTDIR)),
            _ => {
                warn!("virtio-fsd: readdir: invalid fd '{fd}'");
                Err(Error::new(EBADF))
            }
        }
    }

    pub async fn rm(&mut self, mut walk: WalkResult, is_dir: bool, caller: Caller) -> Result<()> {
        let name = walk.rel_path.pop().unwrap_or_default();

        if name.is_empty() {
            return Err(Error::new(EINVAL));
        }

        if let Some(inode) = self.inodes.get_mut(&walk.nodeid) {
            let name_cstring = CString::new(name.clone()).map_err(|_| {
                warn!("virtio-fsd: rm: file name contained an internal 0 byte.");
                Error::new(EINVAL)
            })?;

            let opcode = if is_dir { "FUSE_RMDIR" } else { "FUSE_UNLINK" };

            self.fuse
                .rm(walk.parent_nodeid, name_cstring, is_dir, caller)
                .await
                .map_err(|e| e.into_syscall(opcode))?;

            if inode.open_fd_count == 0 {
                if inode.nlookup > 0 {
                    self.forget_queue.push_back((walk.nodeid, inode.nlookup));
                }
                self.inodes.remove(&walk.nodeid);
            } else {
                inode.forget_pending = true;
            }
        } else {
            return Err(Error::new(ENOENT));
        }

        if let Some(parent) = self.inodes.get_mut(&walk.parent_nodeid) {
            parent.child_count = parent.child_count.saturating_sub(1);
        }
        self.drop_dentry(walk.parent_nodeid, &name);
        self.invalidate_attr(walk.parent_nodeid);
        self.flush_forgets(caller).await;
        Ok(())
    }

    pub async fn rename(&mut self, fd: usize, dest: &str, caller: Caller) -> Result<()> {
        let (src_nodeid, src_path) = match self.handles.get(fd)? {
            Handle::File { nodeid, path, .. }
            | Handle::Directory { nodeid, path, .. }
            | Handle::StatOnly { nodeid, path }
            | Handle::Symlink { nodeid, path, .. } => (*nodeid, path.clone()),
            _ => return Err(Error::new(EBADF)),
        };

        let mut src_parts: Vec<String> = path_components(&src_path);
        let old_name = src_parts.pop().ok_or(Error::new(EINVAL))?;
        let old_parent = if src_parts.is_empty() {
            FUSE_ROOT_ID
        } else {
            let parent_path = format!("/{}", src_parts.join("/"));
            let walk = self.walk_path(fd, &parent_path, 0, caller).await?;
            if walk.last_to_be_created {
                return Err(Error::new(ENOENT));
            }
            walk.nodeid
        };

        let dest_walk = self.walk_path(fd, dest, O_CREAT, caller).await?;
        let new_name = dest_walk
            .rel_path
            .last()
            .cloned()
            .ok_or(Error::new(EINVAL))?;
        let new_parent = dest_walk.parent_nodeid;
        if new_parent == 0 {
            return Err(Error::new(EINVAL));
        }
        let dest_exists = !dest_walk.last_to_be_created;
        let dest_nodeid = dest_walk.nodeid;

        let old_cstring = CString::new(old_name.as_str()).map_err(|_| {
            warn!("virtio-fsd: rename: old name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;
        let new_cstring = CString::new(new_name.as_str()).map_err(|_| {
            warn!("virtio-fsd: rename: new name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        self.fuse
            .rename(old_parent, old_cstring, new_parent, new_cstring, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_RENAME"))?;

        // forgetting dest inode if it was replaced
        if dest_exists && dest_nodeid != src_nodeid {
            if let Some(inode) = self.inodes.get_mut(&dest_nodeid) {
                if inode.open_fd_count == 0 {
                    if inode.nlookup > 0 {
                        self.forget_queue.push_back((dest_nodeid, inode.nlookup));
                    }
                    self.inodes.remove(&dest_nodeid);
                } else {
                    inode.forget_pending = true;
                }
            }
            if let Some(parent) = self.inodes.get_mut(&new_parent) {
                parent.child_count = parent.child_count.saturating_sub(1);
            }
        }

        if old_parent != new_parent {
            if let Some(parent) = self.inodes.get_mut(&old_parent) {
                parent.child_count = parent.child_count.saturating_sub(1);
            }
            if let Some(parent) = self.inodes.get_mut(&new_parent) {
                parent.child_count = parent.child_count.saturating_add(1);
            }
            if let Some(inode) = self.inodes.get_mut(&src_nodeid) {
                inode.parent = new_parent;
            }
        }

        self.drop_dentry(old_parent, &old_name);
        self.drop_dentry(new_parent, &new_name);
        self.invalidate_attr(old_parent);
        self.invalidate_attr(new_parent);
        self.invalidate_attr(src_nodeid);

        // rewrite handle paths
        let new_path = dest_walk.rel_path.join("/");
        for handle in self.handles.values_mut() {
            let path = match handle {
                Handle::File { path, .. }
                | Handle::Directory { path, .. }
                | Handle::StatOnly { path, .. }
                | Handle::Symlink { path, .. }
                | Handle::SymlinkPreCreate { path, .. } => path,
                Handle::SchemeRoot => continue,
                #[cfg(feature = "lru-debug")]
                Handle::LruLog => continue,
            };
            if path == &src_path {
                *path = new_path.clone();
            } else if let Some(rest) = path.strip_prefix(&src_path)
                && rest.starts_with('/')
            {
                *path = format!("{new_path}{rest}");
            }
        }

        self.flush_forgets(caller).await;
        Ok(())
    }

    pub async fn readlink(&self, nodeid: u64, caller: Caller) -> Result<String> {
        Ok(self
            .fuse
            .readlink(nodeid, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_READLINK"))?)
    }

    pub fn get_path(&self, fd: usize) -> Result<&str> {
        match self.handles.get(fd)? {
            Handle::File { path, .. }
            | Handle::Directory { path, .. }
            | Handle::StatOnly { path, .. }
            | Handle::Symlink { path, .. }
            | Handle::SymlinkPreCreate { path, .. } => Ok(path),
            Handle::SchemeRoot => Ok(""),
            #[cfg(feature = "lru-debug")]
            Handle::LruLog => Ok(LRU_EVICTED_NAME),
        }
    }

    pub fn insert_stat_only(&mut self, nodeid: u64, path: Vec<String>) -> Result<usize> {
        self.increment_fd_count(nodeid)?;
        Ok(self.handles.insert(Handle::StatOnly {
            nodeid,
            path: path.join("/"),
        }))
    }

    pub fn insert_symlink(
        &mut self,
        nodeid: u64,
        path: Vec<String>,
        target: String,
    ) -> Result<usize> {
        self.increment_fd_count(nodeid)?;
        Ok(self.handles.insert(Handle::Symlink {
            nodeid,
            path: path.join("/"),
            target,
        }))
    }

    pub fn pre_create_symlink(
        &mut self,
        parent_nodeid: u64,
        name: String,
        path: Vec<String>,
    ) -> Result<usize> {
        Ok(self.handles.insert(Handle::SymlinkPreCreate {
            parent_nodeid,
            name,
            path: path.join("/"),
        }))
    }

    pub fn parent_of(&mut self, nodeid: u64) -> Result<(u64, u16)> {
        let inode = self.inodes.get(&nodeid).ok_or(Error::new(ESTALE))?;
        let parent_id = inode.parent;
        if parent_id == nodeid {
            // we don't have the authority to look into the parent of root,
            // hence when cwd is `/scheme/virtio-fsd`, `ls ..` is same as `ls .`.
            self.touch(FUSE_ROOT_ID);
            return Ok((FUSE_ROOT_ID, MODE_DIR));
        }
        let parent = self.inodes.get(&parent_id).ok_or(Error::new(ESTALE))?;
        let out = (parent.nodeid, parent.mode_type());
        self.touch(nodeid);
        self.touch(parent_id);
        Ok(out)
    }
}

fn path_components<'a, T: From<&'a str>>(path: &'a str) -> Vec<T> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(T::from)
        .collect()
}

fn fuse_open_flags(redox_flags: usize, is_dir: bool, for_create: bool) -> Result<u32> {
    let accmode = redox_flags & O_ACCMODE;

    if is_dir && (accmode == O_WRONLY || accmode == O_RDWR) {
        return Err(Error::new(EISDIR));
    }
    if redox_flags & O_DIRECTORY != 0 && !is_dir {
        return Err(Error::new(ENOTDIR));
    }

    let mut linux_flags: u32 = match accmode {
        O_RDWR => linux_abi::O_RDWR,
        O_WRONLY => linux_abi::O_WRONLY,
        _ => linux_abi::O_RDONLY,
    };

    if !is_dir {
        if for_create && redox_flags & O_CREAT != 0 {
            linux_flags |= linux_abi::O_CREAT;
        }
        if for_create && redox_flags & O_EXCL != 0 {
            linux_flags |= linux_abi::O_EXCL;
        }
        if redox_flags & O_APPEND != 0 {
            linux_flags |= linux_abi::O_APPEND;
        }
        if redox_flags & O_TRUNC != 0 {
            linux_flags |= linux_abi::O_TRUNC;
        }
        if redox_flags & O_NONBLOCK != 0 {
            linux_flags |= linux_abi::O_NONBLOCK;
        }
    } else if redox_flags & O_DIRECTORY != 0 {
        linux_flags |= linux_abi::O_DIRECTORY;
    }

    Ok(linux_flags)
}

fn apply_timespec(fuse_in: &mut fuse_setattr_in, ts: &TimeSpec, atime: bool) -> Result<()> {
    match ts.tv_nsec {
        UTIME_OMIT => Ok(()),
        UTIME_NOW => {
            fuse_in.valid |= if atime {
                FATTR_ATIME | FATTR_ATIME_NOW
            } else {
                FATTR_MTIME | FATTR_MTIME_NOW
            };
            Ok(())
        }
        nsec if nsec >= 0 && nsec < 1_000_000_000 && ts.tv_sec >= 0 => {
            if atime {
                fuse_in.valid |= FATTR_ATIME;
                fuse_in.atime = ts.tv_sec as u64;
                fuse_in.atimensec = nsec as u32;
            } else {
                fuse_in.valid |= FATTR_MTIME;
                fuse_in.mtime = ts.tv_sec as u64;
                fuse_in.mtimensec = nsec as u32;
            }
            Ok(())
        }
        _ => Err(Error::new(EINVAL)),
    }
}

/// Cache expiry from a FUSE `*_valid`/`*_valid_nsec` pair. Clamps nsec to
/// `0..=999_999_999` like the kernel does (no carry into seconds, so
/// `Duration::new` can't panic). Saturates to a large TTL if `sec` is
/// huge (e.g. `u64::MAX` for "forever"); if that still overflows, falls
/// back to `now` (zero TTL, not infinite, not true saturation, but safe).
fn fuse_expiry(now: Instant, sec: u64, nsec: u32) -> Instant {
    const NS_PER_SEC: u32 = 1_000_000_000;
    let ttl = Duration::new(sec, nsec.min(NS_PER_SEC - 1));
    now.checked_add(ttl)
        .or_else(|| now.checked_add(Duration::from_secs(u32::MAX as u64)))
        .unwrap_or(now)
}
