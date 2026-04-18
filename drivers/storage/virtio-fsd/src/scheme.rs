use log::warn;
use redox_scheme::scheme::{SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::HandleMap;
use std::io;
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{EBADF, ENOENT, EOPNOTSUPP, ESPIPE, Error, Result};
use syscall::schemev2::NewFdFlags;
use syscall::{
    EACCES, EAGAIN, EBADMSG, EINTR, EIO, ENOTDIR, EWOULDBLOCK, EventFlags, MODE_DIR, MODE_TYPE,
    MapFlags, MunmapFlags, O_STAT, Stat, StatVfs, StdFsCallKind, StdFsCallMeta, TimeSpec,
};

use crate::fuse::{FUSE_ROOT_ID, FuseError, FuseSession, FuseTransporter};

enum Handle {
    File { nodeid: u64, fh: u64 },
    Directory { nodeid: u64, fh: u64 },
    StatOnly { nodeid: u64, dir: bool },
    SchemeRoot,
}

pub struct VirtioFsSchemeInner<'sock, T: FuseTransporter> {
    socket: &'sock Socket,
    handles: HandleMap<Handle>,
    fuse_session: FuseSession<T>,
}

pub struct VirtioFsScheme<'sock, T: FuseTransporter> {
    pub inner: VirtioFsSchemeInner<'sock, T>,
    state: SchemeState,
}

impl<'sock, T: FuseTransporter> VirtioFsScheme<'sock, T> {
    pub fn new(socket: &'sock Socket, fuse_session: FuseSession<T>) -> Self {
        let inner = VirtioFsSchemeInner {
            socket,
            handles: HandleMap::new(),
            fuse_session,
        };

        Self {
            inner,
            state: SchemeState::new(),
        }
    }

    pub fn on_close(&mut self, id: usize) {
        // TODO: send 'FUSE_RELEASE' and 'FUSE_RELEASEDIR', and
        // 'FUSE_FORGET' (for all types) based on close handle's type
        if self.inner.handles.remove(id).is_none() {
            warn!("virtio-fsd: on_close: key '{id}' was not found in handles");
        };
    }

    // pub fn on_sendfd(&mut self, sendfd_request: &SendFdRequest) -> Result<usize> {
    //     warn!(
    //         "virtio-fsd: sendfd is not supported on id {}",
    //         sendfd_request.id()
    //     );
    //     Err(Error::new(EOPNOTSUPP))
    // }

    /// Process pending and new requests.
    ///
    /// This needs to be called each time there is a new event on the scheme.
    pub async fn tick(&mut self) -> io::Result<()> {
        // Handle new scheme requests
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
                    // TODO: Spawn a separate task for each scheme call. This would however require the
                    // use of a smarter buffer pool (or direct IO, or a buffer per fd) in order to do
                    // parallel IO. It might also require async-aware locks so that a close() is
                    // correctly ordered wrt IO on the same fd.
                    call_request
                        .handle_async(&mut self.inner, &mut self.state)
                        .await
                }
                RequestKind::SendFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::RecvFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::Cancellation(_cancellation_request) => {
                    // FIXME implement cancellation
                    continue;
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {
                    unreachable!()
                }
                RequestKind::OnClose { id } => {
                    self.on_close(id);
                    continue;
                }
                RequestKind::OnDetach { id, pid } => todo!(),
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
        warn!("virtio-fsd: openat called with fd '{fd}' and path '{path}'");

        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }

        let mut node_id = FUSE_ROOT_ID;
        let mut is_dir = node_id == FUSE_ROOT_ID;

        // TODO: revalidate path evaluation
        if !path.starts_with("/") {
            match self.handles.get(fd)? {
                Handle::File { .. } => return Err(Error::new(ENOTDIR)),
                Handle::Directory { nodeid, .. } => {
                    node_id = *nodeid;
                    is_dir = true;
                }
                Handle::StatOnly { nodeid, dir } => {
                    node_id = *nodeid;
                    is_dir = *dir;
                }
                Handle::SchemeRoot => {}
            };
        }

        // Do fuse lookup to find the nodeid of the requested file/dir relative to current nodeid.

        if !path.is_empty() && path != "/" {
            for name in path.split("/") {
                if name.is_empty() || name == "." {
                    continue;
                }

                // TODO: implement a ddirectory cache for performance and parent resolution
                if name == ".." && node_id == FUSE_ROOT_ID {
                    continue;
                }

                match self.fuse_session.lookup(node_id, name).await {
                    Ok(response) => {
                        let payload = response.out_payload.ok_or_else(|| {
                            warn!("virtio-fsd: FUSE LOOKUP response is missing the payload.");
                            return Error::new(EIO);
                        })?;

                        // TODO: save attrs and valid durations, etc.
                        node_id = payload.nodeid;
                        is_dir = (payload.attr.mode as u16 & MODE_TYPE) == MODE_DIR;
                    }
                    Err(err) => match err {
                        FuseError::HostError(code) => {
                            return Err(Error::new((-code) as i32));
                        }
                        FuseError::Transport(_) | FuseError::MalformedPayload => {
                            warn!("virtio-fsd: FUSE LOOKUP transport/protocol err: {}", err);
                            return Err(Error::new(EIO));
                        }
                    },
                }
            }
        }

        if flags & O_STAT == O_STAT {
            let fd = self.handles.insert(Handle::StatOnly {
                nodeid: node_id,
                dir: is_dir,
            });

            return Ok(OpenResult::ThisScheme {
                number: fd,
                flags: NewFdFlags::POSITIONED,
            });
        }

        // TODO: 'O_CREAT'

        match self.fuse_session.open(node_id, flags as u32, is_dir).await {
            Ok(response) => {
                let fh = response
                    .out_payload
                    .ok_or_else(|| {
                        warn!("virtio-fsd: FUSE OPEN: response is missing the payload.");
                        Error::new(EIO)
                    })?
                    .fh;

                let handle = if is_dir {
                    Handle::Directory {
                        nodeid: node_id,
                        fh,
                    }
                } else {
                    Handle::File {
                        nodeid: node_id,
                        fh,
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
                    return Err(Error::new((-code) as i32));
                }
                FuseError::Transport(_) | FuseError::MalformedPayload => {
                    warn!("virtio-fsd: FUSE OPENDIR transport/protocol err: {}", err);
                    return Err(Error::new(EIO));
                }
            },
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
        warn!("virtio-fsd: read is not supported on id {id}");
        Err(Error::new(EBADF))
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        warn!("virtio-fsd: write is not supported on id {id}");
        Err(Error::new(EBADF))
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
        warn!("virtio-fsd: fpath is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn frename(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        warn!("virtio-fsd: frename is not supported on id {id} and path {path}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, ctx: &CallerCtx) -> Result<()> {
        let (nodeid, fh) = match self.handles.get(id)? {
            Handle::File { nodeid, fh } | Handle::Directory { nodeid, fh } => (*nodeid, Some(*fh)),
            Handle::StatOnly { nodeid, .. } => (*nodeid, None),
            _ => {
                warn!("virtio-fsd: fstat is called on an invalid id: {id}");
                return Err(Error::new(EBADF));
            }
        };

        match self.fuse_session.getattr(nodeid, fh).await {
            Ok(response) => {
                let getattr_out = response.out_payload.ok_or_else(|| {
                    warn!("virtio-fsd: FUSE GETATTR response is missing the payload.");
                    Error::new(EIO)
                })?;

                // TODO:
                // pub attr_valid: u64, /* Cache timeout for the attributes */
                // pub attr_valid_nsec: u32,

                let host_attr = getattr_out.attr;

                *stat = Stat {
                    // the device ID of the filesystem itself. we are the filesystem, so we just invent a unique id.
                    // if multiple instances of this filesystem are possible, an atomically incremented value is needed.
                    st_dev: 1,
                    st_ino: host_attr.ino,
                    st_mode: host_attr.mode as u16,

                    st_nlink: host_attr.nlink,
                    st_uid: host_attr.uid,
                    st_gid: host_attr.gid,
                    st_size: host_attr.size,
                    st_blksize: host_attr.blksize,
                    st_blocks: host_attr.blocks,

                    st_atime: host_attr.atime,
                    st_atime_nsec: host_attr.atimensec,
                    st_mtime: host_attr.mtime,
                    st_mtime_nsec: host_attr.mtimensec,
                    st_ctime: host_attr.ctime,
                    st_ctime_nsec: host_attr.ctimensec,
                };

                return Ok(());
            }
            Err(err) => match err {
                FuseError::HostError(code) => {
                    return Err(Error::new((-code) as i32));
                }
                FuseError::Transport(_) | FuseError::MalformedPayload => {
                    warn!("virtio-fsd: FUSE GETATTR transport/protocol err: {}", err);
                    return Err(Error::new(EIO));
                }
            },
        }
    }

    async fn fstatvfs(&mut self, id: usize, stat: &mut StatVfs, ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: fstatvfs is not supported on id {id}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fsync(&mut self, id: usize, ctx: &CallerCtx) -> Result<()> {
        warn!("virtio-fsd: fsync is not supported on id {id}");
        // Err(Error::new(EOPNOTSUPP))
        Ok(())
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
        Err(Error::new(EOPNOTSUPP))
    }

    async fn getdents<'buf>(
        &mut self,
        id: usize,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        let (nodeid, fh) = match self.handles.get(id)? {
            Handle::File { nodeid, fh } | Handle::Directory { nodeid, fh } => (*nodeid, *fh),
            _ => {
                warn!("virtio-fsd: getdents was called with invalid id: {id}");
                return Err(Error::new(EBADF));
            }
        };

        match self.fuse_session.readdir(nodeid, fh, opaque_offset).await {
            Ok(response) => {
                if let Some(payload) = response.out_payload {
                    for dir in payload.into_iter(response.written_len as usize) {
                        let name = match str::from_utf8(dir.name) {
                            Ok(n) => n,
                            Err(err) => {
                                warn!(
                                    "virtio-fsd: FUSE READDIR: name is not valid to utf8: {}",
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
                } else {
                    warn!("virtio-fsd: FUSE READDIR returned empty payload.");
                    Err(Error::new(EIO))
                }
            }
            Err(err) => match err {
                FuseError::HostError(code) => {
                    return Err(Error::new((-code) as i32));
                }
                FuseError::Transport(_) | FuseError::MalformedPayload => {
                    warn!("virtio-fsd: FUSE READDIR transport/protocol err: {}", err);
                    return Err(Error::new(EIO));
                }
            },
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
