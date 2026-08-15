use std::{
    collections::BTreeMap,
    ffi::CString,
    time::{Duration, Instant},
};

use log::warn;
use scheme_utils::HandleMap;
use syscall::{
    EBADF, EINVAL, EISDIR, ENOENT, ENOTDIR,
    error::{ESTALE, Error, Result},
};
use syscall::{MODE_DIR, MODE_TYPE};

use crate::fuse::{
    FUSE_ROOT_ID, FuseReaddirOut, FuseSession, FuseTransporter, fuse_attr, fuse_attr_out,
    fuse_entry_out,
};

struct Inode {
    nodeid: u64,
    generation: Option<u64>,
    parent: u64,
    attr: fuse_attr,
    attr_expiry: Instant,
    nlookup: u64,
    forget_pending: bool,
    open_fd_count: u64,
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
    File { nodeid: u64, fh: u64, path: String },
    Directory { nodeid: u64, fh: u64, path: String },
    StatOnly { nodeid: u64, path: String },
    SchemeRoot,
}

pub struct Vfs<T: FuseTransporter> {
    fuse: FuseSession<T>,
    inodes: BTreeMap<u64, Inode>,
    dcache: BTreeMap<u64, BTreeMap<String, Dentry>>,
    handles: HandleMap<Handle>,
    // TODO: add forget queue
}

impl<T: FuseTransporter> Vfs<T> {
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
            },
        );

        Self {
            fuse: fuse_session,
            inodes,
            dcache: BTreeMap::new(),
            handles: HandleMap::new(),
        }
    }

    fn merge_entry(&mut self, parent: u64, name: &str, entry: fuse_entry_out) -> Result<()> {
        let now = Instant::now();
        let attr_expiry = now + Duration::new(entry.attr_valid, entry.attr_valid_nsec);
        let entry_expiry = now + Duration::new(entry.entry_valid, entry.entry_valid_nsec);

        match self.inodes.get_mut(&entry.nodeid) {
            Some(inode) => {
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
                inode.parent = parent;
            }
            None => {
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
                    },
                );
            }
        }

        self.dcache.entry(parent).or_default().insert(
            name.to_owned(),
            Dentry {
                nodeid: entry.nodeid,
                entry_expiry,
            },
        );
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
        match self.inodes.get_mut(&nodeid) {
            Some(inode) if inode.open_fd_count > 0 => inode.open_fd_count -= 1,
            Some(_) => warn!("virtio-fsd: open_fd_count underflow nodeid={nodeid}"),
            None => warn!("virtio-fsd: decrement_fd_count: no inode for nodeid={nodeid}"),
        }
    }

    fn cached_child(&self, parent: u64, name: &str) -> Option<u64> {
        let dentry = self.dcache.get(&parent)?.get(name)?;
        if !dentry.is_valid() || !self.inodes.contains_key(&dentry.nodeid) {
            return None;
        }
        Some(dentry.nodeid)
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
                Instant::now() + Duration::new(payload.attr_valid, payload.attr_valid_nsec);
        }
        mode
    }

    pub async fn lookup(&mut self, parent: u64, name: &str) -> Result<(u64, u16)> {
        if let Some(nodeid) = self.cached_child(parent, name) {
            if let Some(inode) = self.inodes.get(&nodeid)
                && inode.attr_valid()
            {
                return Ok((nodeid, inode.mode_type()));
            }

            let payload = match self.fuse.getattr(nodeid, None).await {
                Ok(payload) => payload,
                Err(e) => {
                    let err = e.into_syscall("FUSE_GETATTR");
                    if err.errno == ENOENT {
                        self.drop_dentry(parent, name);
                    }
                    return Err(err);
                }
            };

            return Ok((nodeid, self.refresh_attr(nodeid, payload)));
        }

        let payload = self
            .fuse
            .lookup(parent, name)
            .await
            .map_err(|e| e.into_syscall("FUSE_LOOKUP"))?;
        let nodeid = payload.nodeid;
        let mode = payload.attr.mode as u16 & MODE_TYPE;
        self.merge_entry(parent, name, payload)?;
        Ok((nodeid, mode))
    }

    pub async fn create(
        &mut self,
        parent_nodeid: u64,
        name: &str,
        flags: u32,
        mode: u32,
        path: Vec<String>,
    ) -> Result<usize> {
        let name_cstring = CString::new(name).map_err(|_| {
            warn!("virtio-fsd: openat: file name contained an internal 0 byte.");
            Error::new(EINVAL)
        })?;

        let payload = self
            .fuse
            .create(parent_nodeid, name_cstring, flags, mode)
            .await
            .map_err(|e| e.into_syscall("FUSE_CREATE"))?;

        self.invalidate_attr(parent_nodeid);

        let nodeid = payload.entry.nodeid;

        if let Err(err) = self.merge_entry(parent_nodeid, name, payload.entry) {
            if let Err(rel_err) = self.fuse.release(nodeid, payload.open.fh, false).await {
                warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
            }
            return Err(err);
        }

        if let Err(err) = self.increment_fd_count(nodeid) {
            if let Err(rel_err) = self.fuse.release(nodeid, payload.open.fh, false).await {
                warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
            }
            return Err(err);
        }

        Ok(self.handles.insert(Handle::File {
            nodeid,
            fh: payload.open.fh,
            path: path.join("/"),
        }))
    }

    pub async fn open(
        &mut self,
        nodeid: u64,
        flags: u32,
        is_dir: bool,
        path: Vec<String>,
    ) -> Result<usize> {
        let opcode = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };

        let payload = self
            .fuse
            .open(nodeid, flags, is_dir)
            .await
            .map_err(|e| e.into_syscall(opcode))?;

        let fh = payload.fh;

        if let Err(err) = self.increment_fd_count(nodeid) {
            if let Err(rel_err) = self.fuse.release(nodeid, fh, is_dir).await {
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

        Ok(self.handles.insert(handle))
    }

    pub async fn getattr(&mut self, fd: usize) -> Result<fuse_attr> {
        let (nodeid, fh) = match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } => (*nodeid, None),
            Handle::SchemeRoot => (FUSE_ROOT_ID, None),
        };

        if let Some(inode) = self.inodes.get(&nodeid)
            && inode.attr_valid()
        {
            return Ok(inode.attr);
        }

        let payload = self
            .fuse
            .getattr(nodeid, fh)
            .await
            .map_err(|e| e.into_syscall("FUSE_GETATTR"))?;

        self.refresh_attr(nodeid, payload);

        return Ok(payload.attr);
    }

    pub async fn read(&mut self, fd: usize, buf: &mut [u8], offset: u64) -> Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }

        match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => self
                .fuse
                .read(*nodeid, *fh, offset, buf, 0, 0)
                .await
                .map_err(|e| e.into_syscall("FUSE_READ")),
            Handle::Directory { .. } => Err(Error::new(EISDIR)),
            _ => {
                warn!("virtio-fsd: read: fd '{fd}' is not associated with a valid file.");
                Err(Error::new(EBADF))
            }
        }
    }

    pub async fn write(&mut self, fd: usize, buf: &[u8], offset: u64) -> Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }

        match self.handles.get(fd)? {
            Handle::File { nodeid, fh, .. } => {
                let nodeid = *nodeid;
                let fh = *fh;
                let payload = self
                    .fuse
                    .write(nodeid, fh, offset, buf, 0, 0)
                    .await
                    .map_err(|e| e.into_syscall("FUSE_WRITE"))?;
                self.invalidate_attr(nodeid);
                Ok(payload.size as usize)
            }
            Handle::Directory { .. } => Err(Error::new(EISDIR)),
            _ => {
                warn!("virtio-fsd: write: fd '{fd}' is not associated with a valid file.");
                Err(Error::new(EBADF))
            }
        }
    }

    pub async fn fsync(&self, fd: usize) -> Result<()> {
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
            .fsync(nodeid, fh, is_dir)
            .await
            .map_err(|e| e.into_syscall(opcode))
    }

    pub async fn close(&mut self, fd: usize) {
        match self.handles.remove(fd) {
            Some(handle) => match handle {
                Handle::File { nodeid, fh, .. } => {
                    if let Err(err) = self.fuse.release(nodeid, fh, false).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.decrement_fd_count(nodeid);
                }
                Handle::Directory { nodeid, fh, .. } => {
                    if let Err(err) = self.fuse.release(nodeid, fh, true).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.decrement_fd_count(nodeid);
                }
                Handle::StatOnly { nodeid, .. } => {
                    self.decrement_fd_count(nodeid);
                }
                Handle::SchemeRoot => {}
            },
            None => {
                warn!("virtio-fsd: close: fd '{fd}' was not found in handles");
            }
        };
    }

    pub fn scheme_root(&mut self) -> usize {
        self.handles.insert(Handle::SchemeRoot)
    }

    pub fn walk_start(&self, fd: usize) -> Result<(u64, u16, Vec<String>)> {
        match self.handles.get(fd)? {
            Handle::File { .. } => Err(Error::new(ENOTDIR)),
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
        }
    }

    fn inode_mode(&self, nodeid: u64) -> Result<u16> {
        self.inodes
            .get(&nodeid)
            .map(Inode::mode_type)
            .ok_or(Error::new(ESTALE))
    }

    pub async fn readdir(&mut self, fd: usize, offset: u64) -> Result<FuseReaddirOut> {
        match self.handles.get(fd)? {
            Handle::Directory { nodeid, fh, .. } => {
                let (nodeid, fh) = (*nodeid, *fh);
                self.fuse
                    .readdir(nodeid, fh, offset)
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

    pub fn get_path(&self, fd: usize) -> Result<&str> {
        match self.handles.get(fd)? {
            Handle::File { path, .. }
            | Handle::Directory { path, .. }
            | Handle::StatOnly { path, .. } => Ok(path),
            Handle::SchemeRoot => Ok(""),
        }
    }

    pub fn insert_stat_only(&mut self, nodeid: u64, path: Vec<String>) -> Result<usize> {
        self.increment_fd_count(nodeid)?;
        Ok(self.handles.insert(Handle::StatOnly {
            nodeid,
            path: path.join("/"),
        }))
    }

    pub fn parent_of(&self, nodeid: u64) -> Result<(u64, u16)> {
        let inode = self.inodes.get(&nodeid).ok_or(Error::new(ESTALE))?;
        let parent_id = inode.parent;
        if parent_id == nodeid {
            // we don't have the authority to look into the parent of root,
            // hence when cwd is `/scheme/virtio-fsd`, `ls ..` is same as `ls .`.
            return Ok((FUSE_ROOT_ID, MODE_DIR));
        }
        let parent = self.inodes.get(&parent_id).ok_or(Error::new(ESTALE))?;
        Ok((parent.nodeid, parent.mode_type()))
    }

    fn invalidate_attr(&mut self, nodeid: u64) {
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.attr_expiry = Instant::now();
        }
    }
}

pub(crate) fn path_components<'a, T: From<&'a str>>(path: &'a str) -> Vec<T> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(T::from)
        .collect()
}
