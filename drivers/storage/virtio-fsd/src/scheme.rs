use log::warn;
use redox_scheme::scheme::{SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::HandleMap;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::time::{Duration, Instant};
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{EBADF, ENOENT, EOPNOTSUPP, ESPIPE, Error, Result};
use syscall::schemev2::NewFdFlags;
use syscall::{
    EAGAIN, EBADMSG, EINTR, EIO, EISDIR, ELOOP, ENOTDIR, EWOULDBLOCK, EventFlags, MODE_DIR,
    MODE_SYMLINK, MODE_TYPE, MapFlags, MunmapFlags, Stat, StatVfs, StdFsCallKind, StdFsCallMeta,
    TimeSpec,
};

use crate::fuse::{FUSE_ROOT_ID, FuseAttr, FuseError, FuseSession, FuseTransporter};
use crate::linux_abi;

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
        mode: u16,
        path: String,
    },
    SchemeRoot,
}

pub struct VirtioFsSchemeInner<'sock, T: FuseTransporter> {
    socket: &'sock Socket,
    handles: HandleMap<Handle>,
    fuse_session: FuseSession<T>,
    /// Maps `child nodeid` -> `parent nodeid`. This is used to resolve `nodeid` for `..`.
    parent_map: BTreeMap<u64, u64>,
    /// Directory cache: Maps `parent nodeid` to a map of its `child name as Vec<u8>` -> `(child nodeid, expiration)`.
    dcache: BTreeMap<u64, BTreeMap<Vec<u8>, (u64, Instant)>>,
    /// Attribute cache: Maps `nodeid` -> `(attrs, expiration)`.
    acache: BTreeMap<u64, (FuseAttr, Instant)>,
}

pub struct VirtioFsScheme<'sock, T: FuseTransporter> {
    pub inner: VirtioFsSchemeInner<'sock, T>,
    state: SchemeState,
}

impl<'sock, T: FuseTransporter> VirtioFsScheme<'sock, T> {
    pub fn new(socket: &'sock Socket, fuse_session: FuseSession<T>) -> Self {
        // we don't have the authority to look into the parent of root,
        // hence when cwd is `/scheme/virtio-fsd`, `ls ..` is same as `ls .`.
        let mut parent_map = BTreeMap::new();
        parent_map.insert(FUSE_ROOT_ID, FUSE_ROOT_ID);

        let inner = VirtioFsSchemeInner {
            fuse_session,
            socket,
            handles: HandleMap::new(),
            parent_map,
            dcache: BTreeMap::new(),
            acache: BTreeMap::new(),
        };

        Self {
            inner,
            state: SchemeState::new(),
        }
    }

    pub async fn on_close(&mut self, id: usize) {
        match self.inner.handles.remove(id) {
            Some(handle) => match handle {
                Handle::File { nodeid, fh, .. } => {
                    if let Err(err) = self.inner.fuse_session.release(nodeid, fh, false).await {
                        match err {
                            FuseError::HostError(code) => {
                                warn!("virtio-fsd: FUSE_LOOKUP host err: {}", -code);
                            }
                            FuseError::Transport(_) | FuseError::MalformedPayload => {
                                warn!("virtio-fsd: FUSE_LOOKUP transport/protocol err: {}", err);
                            }
                        }
                    }
                }
                Handle::Directory { nodeid, fh, .. } => {
                    if let Err(err) = self.inner.fuse_session.release(nodeid, fh, true).await {
                        match err {
                            FuseError::HostError(code) => {
                                warn!("virtio-fsd: FUSE_LOOKUP host err: {}", -code);
                            }
                            FuseError::Transport(_) | FuseError::MalformedPayload => {
                                warn!("virtio-fsd: FUSE_LOOKUP transport/protocol err: {}", err);
                            }
                        }
                    }
                }
                Handle::StatOnly { .. } | Handle::SchemeRoot => {}
            },
            None => {
                warn!("virtio-fsd: on_close: key '{id}' was not found in handles");
            }
        };
    }

    pub async fn tick(&mut self) -> io::Result<()> {
        loop {
            let request = match self.inner.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    // Scheme likely got unmounted
                    // TODO: return this to caller instead
                    std::process::exit(0);
                }
                Err(error) if error.errno == EWOULDBLOCK || error.errno == EAGAIN => break,
                Err(err) if err.errno == EINTR => continue,
                Err(err) => return Err(err.into()),
            };

            let response = match request.kind() {
                RequestKind::Call(call_request) => {
                    call_request
                        .handle_async(&mut self.inner, &mut self.state)
                        .await
                }
                RequestKind::SendFd(request) => {
                    warn!("virtio-fsd: RequestKind::SendFd not supported");
                    Response::err(EOPNOTSUPP, request)
                }
                RequestKind::RecvFd(request) => {
                    warn!("virtio-fsd: RequestKind::RecvFd not supported");
                    Response::err(EOPNOTSUPP, request)
                }
                RequestKind::Cancellation(_cancellation_request) => {
                    warn!("virtio-fsd: RequestKind::Cancellation not supported");
                    continue;
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {
                    warn!("virtio-fsd: RequestKind::MsyncMsg | MunmapMsg | MmapMsg not supported");
                    continue;
                }
                RequestKind::OnClose { id } => {
                    self.on_close(id).await;
                    continue;
                }
                RequestKind::OnDetach { .. } => {
                    warn!("virtio-fsd: RequestKind::OnDetach not supported");
                    continue;
                }
            };
            self.inner
                .socket
                .write_response(response, SignalBehavior::Restart)?;
        }

        Ok(())
    }
}

impl<'sock, T: FuseTransporter> SchemeAsync for VirtioFsSchemeInner<'sock, T> {
    fn scheme_root(&mut self) -> Result<usize> {
        let id = self.handles.insert(Handle::SchemeRoot);
        Ok(id)
    }

    async fn openat(
        &mut self,
        fd: usize,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        // TODO: check permissions

        let mut node_id = FUSE_ROOT_ID;
        let mut node_mode = MODE_DIR;

        // TODO: revalidate path evaluation
        if !path.starts_with("/") {
            match self.handles.get(fd)? {
                Handle::File { nodeid, fh, .. } => {
                    // TODO
                    warn!("virtio-fsd: file op not suported yet - nodeid: '{nodeid}' - fh: '{fh}'");
                    return Err(Error::new(EOPNOTSUPP));
                }
                Handle::Directory { nodeid, .. } => {
                    node_id = *nodeid;
                    node_mode = MODE_DIR;
                }
                Handle::StatOnly { nodeid, mode, .. } => {
                    node_id = *nodeid;
                    node_mode = *mode;
                }
                Handle::SchemeRoot => {}
            };
        }

        let path_parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let mut target_name_to_create = None;

        if !path.is_empty() && path != "/" {
            for (i, name) in path_parts.iter().enumerate() {
                let is_last_element = i == path_parts.len() - 1;

                if name.is_empty() || *name == "." {
                    continue;
                }

                if *name == ".." {
                    // TODO: teleport to root can happen if driver restarts
                    node_id = *self.parent_map.get(&node_id).unwrap_or(&FUSE_ROOT_ID);
                    node_mode = MODE_DIR;
                    continue;
                }

                if let Some(child_map) = self.dcache.get_mut(&node_id)
                    && let Some((child_nodeid, dexp)) = child_map.get(name.as_bytes())
                {
                    let dexpired = Instant::now() >= *dexp;

                    if let Some((attr, aexp)) = self.acache.get(child_nodeid) {
                        let aexpired = Instant::now() >= *aexp;

                        if !dexpired && !aexpired {
                            node_id = *child_nodeid;
                            node_mode = attr.mode as u16 & MODE_TYPE;
                            continue;
                        } else {
                            self.acache.remove(child_nodeid);
                            self.parent_map.remove(child_nodeid);
                            child_map.remove(name.as_bytes());
                        }
                    }
                }

                // cache miss
                match self.fuse_session.lookup(node_id, name).await {
                    Ok(payload) => {
                        self.parent_map.insert(payload.nodeid, node_id);

                        let owned_name = name.as_bytes().to_vec();
                        let dexp = Instant::now()
                            + Duration::new(payload.entry_valid, payload.entry_valid_nsec);
                        self.dcache
                            .entry(node_id)
                            .or_insert_with(BTreeMap::new)
                            .insert(owned_name, (payload.nodeid, dexp));

                        let aexp = Instant::now()
                            + Duration::new(payload.attr_valid, payload.attr_valid_nsec);
                        self.acache.insert(payload.nodeid, (payload.attr, aexp));

                        node_id = payload.nodeid;
                        node_mode = payload.attr.mode as u16 & MODE_TYPE;
                    }
                    Err(err) => match err {
                        FuseError::HostError(code) => {
                            if code == -ENOENT && is_last_element && (flags & syscall::O_CREAT != 0)
                            {
                                target_name_to_create = Some(*name);
                                break;
                            }
                            return Err(Error::new((-code) as i32));
                        }
                        FuseError::Transport(_) | FuseError::MalformedPayload => {
                            warn!("virtio-fsd: FUSE_LOOKUP transport/protocol err: {}", err);
                            return Err(Error::new(EIO));
                        }
                    },
                }
            }
        }

        if node_mode == MODE_SYMLINK {
            // fall thorugh if stat only
            if flags & syscall::O_STAT == 0 {
                if flags & syscall::O_NOFOLLOW != 0 {
                    warn!("virtio-fsd: O_NOFOLLOW enforced. Refusing to open symlink.");
                    return Err(Error::new(ELOOP));
                } else {
                    // TODO: implement FUSE_READLINK
                    warn!("virtio-fsd: symlink following not implemented yet.");
                    return Err(Error::new(EOPNOTSUPP));
                }
            }
        }

        if flags & syscall::O_STAT != 0 {
            let fd = self.handles.insert(Handle::StatOnly {
                nodeid: node_id,
                mode: node_mode,
                path: path.to_owned(),
            });

            return Ok(OpenResult::ThisScheme {
                number: fd,
                flags: NewFdFlags::POSITIONED,
            });
        }

        if let Some(file_name) = target_name_to_create {
            // We broke out of the loop early, so `node_id` is currently
            // the parent directory where the new file should be created.

            let is_dir = false;
            let open_flags = translate_redox_flags_to_fuse(flags, is_dir)?;

            let name = CString::new(file_name)
                .expect("virtio-fsd: openat: file name contained an internal 0 byte.");

            //TODO: clean
            let redox_mode = (flags & 0xFFFF) as u32;
            const S_IFREG: u32 = 0o100000;
            let final_mode = redox_mode | S_IFREG;

            match self
                .fuse_session
                .create(node_id, name, open_flags, final_mode)
                .await
            {
                Ok(payload) => {
                    //TODO: DRY it

                    // eq to lookup

                    self.parent_map.insert(payload.entry.nodeid, node_id);

                    let owned_name = file_name.as_bytes().to_vec();
                    let dexp = Instant::now()
                        + Duration::new(payload.entry.entry_valid, payload.entry.entry_valid_nsec);
                    self.dcache
                        .entry(node_id)
                        .or_insert_with(BTreeMap::new)
                        .insert(owned_name, (payload.entry.nodeid, dexp));

                    let aexp = Instant::now()
                        + Duration::new(payload.entry.attr_valid, payload.entry.attr_valid_nsec);
                    self.acache
                        .insert(payload.entry.nodeid, (payload.entry.attr, aexp));

                    // eq to open

                    let handle = Handle::File {
                        nodeid: payload.entry.nodeid,
                        fh: payload.open.fh,
                        path: path.to_owned(),
                    };

                    let fd = self.handles.insert(handle);

                    return Ok(OpenResult::ThisScheme {
                        number: fd,
                        flags: NewFdFlags::POSITIONED,
                    });
                }
                Err(err) => match err {
                    FuseError::HostError(code) => {
                        warn!("virtio-fsd: FUSE_CREATE HostError {code}");
                        return Err(Error::new((-code) as i32));
                    }
                    FuseError::Transport(_) | FuseError::MalformedPayload => {
                        warn!("virtio-fsd: FUSE_CREATE transport/protocol err: {}", err);
                        return Err(Error::new(EIO));
                    }
                },
            }
        } else {
            let is_dir = node_mode == MODE_DIR;
            let open_flags = translate_redox_flags_to_fuse(flags, is_dir)?;

            match self.fuse_session.open(node_id, open_flags, is_dir).await {
                Ok(payload) => {
                    let fh = payload.fh;

                    let handle = if is_dir {
                        Handle::Directory {
                            nodeid: node_id,
                            fh,
                            path: path.to_owned(),
                        }
                    } else {
                        Handle::File {
                            nodeid: node_id,
                            fh,
                            path: path.to_owned(),
                        }
                    };

                    let fd = self.handles.insert(handle);

                    return Ok(OpenResult::ThisScheme {
                        number: fd,
                        flags: NewFdFlags::POSITIONED,
                    });
                }
                Err(err) => match err {
                    FuseError::HostError(code) => {
                        let op = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };
                        warn!("virtio-fsd: {op} HostError {code}");
                        return Err(Error::new((-code) as i32));
                    }
                    FuseError::Transport(_) | FuseError::MalformedPayload => {
                        let op = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };
                        warn!("virtio-fsd: {op} transport/protocol err: {}", err);
                        return Err(Error::new(EIO));
                    }
                },
            }
        }
    }

    async fn unlinkat(
        &mut self,
        fd: usize,
        path: &str,
        flags: usize,
        ctx: &CallerCtx,
    ) -> Result<()> {
        warn!("virtio-fsd: unlinkat is not supported on path {path}");
        Err(Error::new(ENOENT))
    }

    /* Resource operations */
    async fn dup(&mut self, old_id: usize, buf: &[u8], ctx: &CallerCtx) -> Result<OpenResult> {
        warn!("virtio-fsd: dup is not supported on id {old_id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        //TODO: utilise ctx

        if buf.len() == 0 {
            return Ok(0);
        }

        if let Handle::File { nodeid, fh, .. } = self.handles.get(id)? {
            match self
                .fuse_session
                .read(*nodeid, *fh, offset, buf, 0, 0)
                .await
            {
                Ok(bytes_read) => {
                    return Ok(bytes_read);
                }
                Err(err) => match err {
                    FuseError::HostError(code) => {
                        warn!("virtio-fsd: FUSE_READ HostError {code}");
                        return Err(Error::new((-code) as i32));
                    }
                    FuseError::Transport(_) | FuseError::MalformedPayload => {
                        warn!("virtio-fsd: FUSE_READ transport/protocol err: {}", err);
                        return Err(Error::new(EIO));
                    }
                },
            }
        } else {
            warn!("virtio-fsd: read: id '{id}' is not associated with a valid file.");
            return Err(Error::new(EBADF));
        }
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        //TODO: utilise ctx

        if buf.len() == 0 {
            return Ok(0);
        }

        if let Handle::File { nodeid, fh, .. } = self.handles.get(id)? {
            match self
                .fuse_session
                .write(*nodeid, *fh, offset, buf, 0, 0)
                .await
            {
                Ok(response) => {
                    return Ok(response.size as usize);
                }
                Err(err) => match err {
                    FuseError::HostError(code) => {
                        warn!("virtio-fsd: FUSE_WRITE HostError {code}");
                        return Err(Error::new((-code) as i32));
                    }
                    FuseError::Transport(_) | FuseError::MalformedPayload => {
                        warn!("virtio-fsd: FUSE_WRITE transport/protocol err: {}", err);
                        return Err(Error::new(EIO));
                    }
                },
            }
        } else {
            warn!("virtio-fsd: write: id '{id}' is not associated with a valid file.");
            return Err(Error::new(EBADF));
        }
    }

    async fn fsize(&mut self, id: usize, ctx: &CallerCtx) -> Result<u64> {
        warn!("virtio-fsd: fsize is not supported on id {id}");
        Err(Error::new(ESPIPE))
    }

    async fn fchmod(&mut self, id: usize, new_mode: u16, ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: fchmod is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fchown(
        &mut self,
        id: usize,
        new_uid: u32,
        new_gid: u32,
        ctx: &CallerCtx,
    ) -> Result<()> {
        warn!("virtio-fsd: fchown is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fcntl(&mut self, id: usize, cmd: usize, arg: usize, ctx: &CallerCtx) -> Result<usize> {
        warn!("virtio-fsd: fcntl is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fevent(
        &mut self,
        id: usize,
        flags: EventFlags,
        ctx: &CallerCtx,
    ) -> Result<EventFlags> {
        warn!("virtio-fsd: fevent is not supported on id {id}");
        Ok(EventFlags::empty())
    }

    async fn flink(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        warn!("virtio-fsd: flink is not supported on id {id} and path {path}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fpath(&mut self, id: usize, buf: &mut [u8], ctx: &CallerCtx) -> Result<usize> {
        // TODO: openat must construct and save absolute paths.
        match self.handles.get(id)? {
            Handle::File { path, .. }
            | Handle::Directory { path, .. }
            | Handle::StatOnly { path, .. } => {
                let path_bytes = path.as_bytes();
                buf[..path_bytes.len()].copy_from_slice(path_bytes);
                return Ok(path_bytes.len());
            }
            _ => {
                warn!("virtio-fsd: fpath: id '{id}' is not associated with a valid file.");
                return Err(Error::new(EBADF));
            }
        };
    }

    async fn frename(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        warn!("virtio-fsd: frename is not supported on id {id} and path {path}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, ctx: &CallerCtx) -> Result<()> {
        let (nodeid, fh) = match self.handles.get(id)? {
            Handle::File { nodeid, fh, .. } | Handle::Directory { nodeid, fh, .. } => {
                (*nodeid, Some(*fh))
            }
            Handle::StatOnly { nodeid, .. } => (*nodeid, None),
            _ => {
                warn!("virtio-fsd: fstat is called on an invalid id: {id}");
                return Err(Error::new(EBADF));
            }
        };

        let attrs = if let Some((attr, exp)) = self.acache.get(&nodeid)
            && Instant::now() < *exp
        {
            *attr
        } else {
            self.acache.remove(&nodeid);

            match self.fuse_session.getattr(nodeid, fh).await {
                Ok(payload) => {
                    let exp =
                        Instant::now() + Duration::new(payload.attr_valid, payload.attr_valid_nsec);
                    self.acache.insert(nodeid, (payload.attr, exp));

                    payload.attr
                }
                Err(err) => match err {
                    FuseError::HostError(code) => {
                        warn!("virtio-fsd: FUSE_GETATTR HostError {code}");
                        return Err(Error::new((-code) as i32));
                    }
                    FuseError::Transport(_) | FuseError::MalformedPayload => {
                        warn!("virtio-fsd: FUSE_GETATTR transport/protocol err: {}", err);
                        return Err(Error::new(EIO));
                    }
                },
            }
        };

        *stat = Stat {
            // the device ID of the filesystem itself. we are the filesystem, so we just invent a unique id.
            // if multiple instances of this filesystem are possible, an atomically incremented value is needed.
            st_dev: 1,
            st_ino: attrs.ino,
            st_mode: attrs.mode as u16,

            st_nlink: attrs.nlink,
            st_uid: attrs.uid,
            st_gid: attrs.gid,
            st_size: attrs.size,
            st_blksize: attrs.blksize,
            st_blocks: attrs.blocks,

            st_atime: attrs.atime,
            st_atime_nsec: attrs.atimensec,
            st_mtime: attrs.mtime,
            st_mtime_nsec: attrs.mtimensec,
            st_ctime: attrs.ctime,
            st_ctime_nsec: attrs.ctimensec,
        };

        return Ok(());
    }

    async fn fstatvfs(&mut self, id: usize, stat: &mut StatVfs, ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: fstatvfs is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fsync(&mut self, id: usize, ctx: &CallerCtx) -> Result<()> {
        let (nodeid, fh, is_dir) = match self.handles.get(id)? {
            Handle::File { nodeid, fh, .. } => (*nodeid, *fh, false),
            Handle::Directory { nodeid, fh, .. } => (*nodeid, *fh, true),
            _ => {
                warn!("virtio-fsd: fsync is called on an invalid id: {id}");
                return Err(Error::new(EBADF));
            }
        };

        match self.fuse_session.fsync(nodeid, fh, is_dir).await {
            Ok(_) => Ok(()),
            Err(err) => match err {
                FuseError::HostError(code) => {
                    warn!("virtio-fsd: FUSE_FSYNC HostError {code}");
                    return Err(Error::new((-code) as i32));
                }
                FuseError::Transport(_) | FuseError::MalformedPayload => {
                    warn!("virtio-fsd: FUSE_FSYNC transport/protocol err: {}", err);
                    return Err(Error::new(EIO));
                }
            },
        }
    }

    async fn ftruncate(&mut self, id: usize, len: u64, ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: ftruncate is not supported on id {id}");
        Err(Error::new(EBADF))
    }

    async fn futimens(&mut self, id: usize, times: &[TimeSpec], ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: futimens is not supported on id {id}");
        Err(Error::new(EBADF))
    }

    async fn call(
        &mut self,
        id: usize,
        payload: &mut [u8],
        metadata: &[u64],
        ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        warn!("virtio-fsd: call is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn std_fs_call(
        &mut self,
        id: usize,
        kind: StdFsCallKind,
        payload: &mut [u8],
        metadata: StdFsCallMeta,
        ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        warn!("virtio-fsd: std_fs_call is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn getdents<'buf>(
        &mut self,
        id: usize,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        match self.handles.get(id)? {
            Handle::File { nodeid, fh, .. } => {
                warn!("file op not suported yet - nodeid: '{nodeid}' - fh: '{fh}'");
                return Err(Error::new(EBADF));
            }
            Handle::Directory { nodeid, fh, .. } => {
                let (nodeid, fh) = (*nodeid, *fh);
                match self.fuse_session.readdir(nodeid, fh, opaque_offset).await {
                    Ok(payload) => {
                        for dir in payload.iter() {
                            let name = match str::from_utf8(dir.name) {
                                Ok(n) => n,
                                Err(err) => {
                                    warn!(
                                        "virtio-fsd: FUSE_READDIR: name is not valid to utf8: {}",
                                        err
                                    );
                                    return Err(Error::new(EBADMSG));
                                }
                            };
                            let kind = DirentKind::try_from_raw(dir.typ as u8)
                                .unwrap_or(DirentKind::Unspecified);

                            buf.entry(DirEntry {
                                inode: dir.ino,
                                next_opaque_id: dir.off,
                                name,
                                kind,
                            })?;
                        }
                        Ok(buf)
                    }
                    Err(err) => match err {
                        FuseError::HostError(code) => {
                            warn!("virtio-fsd: FUSE_READDIR HostError {code}");
                            return Err(Error::new((-code) as i32));
                        }
                        FuseError::Transport(_) | FuseError::MalformedPayload => {
                            warn!("virtio-fsd: FUSE_READDIR transport/protocol err: {}", err);
                            return Err(Error::new(EIO));
                        }
                    },
                }
            }
            _ => {
                warn!("virtio-fsd: getdents was called with invalid id: {id}");
                return Err(Error::new(EBADF));
            }
        }
    }

    async fn mmap_prep(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        flags: MapFlags,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        warn!("virtio-fsd: mmap_prep is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn munmap(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        flags: MunmapFlags,
        ctx: &CallerCtx,
    ) -> Result<()> {
        warn!("virtio-fsd: munmap is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn on_recvfd(&mut self, recvfd_request: &RecvFdRequest) -> Result<OpenResult> {
        warn!(
            "virtio-fsd: recvfd is not supported on id {}",
            recvfd_request.id()
        );
        Err(Error::new(EOPNOTSUPP))
    }
}

fn translate_redox_flags_to_fuse(redox_flags: usize, is_dir: bool) -> Result<u32> {
    let accmode = redox_flags & syscall::O_ACCMODE;

    if is_dir {
        if accmode == syscall::O_WRONLY || accmode == syscall::O_RDWR {
            return Err(Error::new(EISDIR));
        }
    }

    if (redox_flags & syscall::O_DIRECTORY != 0) && !is_dir {
        return Err(Error::new(ENOTDIR));
    }

    let mut linux_flags: u32 = match accmode {
        syscall::O_RDWR => linux_abi::O_RDWR,
        syscall::O_WRONLY => linux_abi::O_WRONLY,
        _ => linux_abi::O_RDONLY,
    };

    if !is_dir {
        if redox_flags & syscall::O_CREAT != 0 {
            linux_flags |= linux_abi::O_CREAT;
        }

        if redox_flags & syscall::O_EXCL != 0 {
            linux_flags |= linux_abi::O_EXCL;
        }

        if redox_flags & syscall::O_APPEND != 0 {
            linux_flags |= linux_abi::O_APPEND;
        }

        // TODO: needs FUSE_ATOMIC_O_TRUNC flag negotiated
        if redox_flags & syscall::O_TRUNC != 0 {
            linux_flags |= linux_abi::O_TRUNC;
        }

        if redox_flags & syscall::O_NONBLOCK != 0 {
            linux_flags |= linux_abi::O_NONBLOCK;
        }
    } else {
        if redox_flags & syscall::O_DIRECTORY != 0 {
            linux_flags |= linux_abi::O_DIRECTORY;
        }
    }

    Ok(linux_flags)
}
