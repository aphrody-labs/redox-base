use log::warn;
use redox_scheme::scheme::{SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::FpathWriter;
use std::io;
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{EBADF, ENOENT, EOPNOTSUPP, ESPIPE, Error, Result};
use syscall::schemev2::NewFdFlags;
use syscall::{
    EAGAIN, EBADMSG, EEXIST, EINTR, EISDIR, ELOOP, ENOTDIR, EWOULDBLOCK, EventFlags, MODE_DIR,
    MODE_SYMLINK, MapFlags, MunmapFlags, Stat, StatVfs, StdFsCallKind, StdFsCallMeta, TimeSpec,
};

use crate::fuse::{FUSE_ROOT_ID, FuseTransporter};
use crate::linux_abi;
use crate::vfs::{Vfs, path_components};

pub struct VirtioFsSchemeInner<'sock, T: FuseTransporter> {
    socket: &'sock Socket,
    vfs: Vfs<T>,
    scheme_name: String,
}

pub struct VirtioFsScheme<'sock, T: FuseTransporter> {
    pub inner: VirtioFsSchemeInner<'sock, T>,
    state: SchemeState,
}

impl<'sock, T: FuseTransporter> VirtioFsScheme<'sock, T> {
    pub fn new(socket: &'sock Socket, vfs: Vfs<T>) -> Self {
        let inner = VirtioFsSchemeInner {
            vfs,
            socket,
            scheme_name: "virtio-fsd".to_owned(),
        };

        Self {
            inner,
            state: SchemeState::new(),
        }
    }

    pub async fn on_close(&mut self, id: usize) {
        self.inner.vfs.close(id).await
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
        Ok(self.vfs.scheme_root())
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

        // TODO: revalidate path evaluation
        let (mut nodeid, mut node_mode, mut rel_path) = if path.starts_with('/') {
            (FUSE_ROOT_ID, MODE_DIR, Vec::new())
        } else {
            self.vfs.walk_start(fd)?
        };

        let path_parts: Vec<&str> = path_components(path);
        #[cfg(feature = "lru-debug")]
        if path_parts.as_slice() == [crate::vfs::LRU_EVICTED_NAME] && nodeid == FUSE_ROOT_ID {
            // O_TRUNC resets the in-memory eviction log so selftests can isolate runs
            // without restarting the daemon.
            let clear = flags & syscall::O_TRUNC != 0;
            return Ok(OpenResult::ThisScheme {
                number: self.vfs.open_lru_log(clear),
                flags: NewFdFlags::POSITIONED,
            });
        }

        let mut to_be_created = None;

        if !path.is_empty() && path != "/" {
            for (i, name) in path_parts.iter().copied().enumerate() {
                let is_last_element = i == path_parts.len() - 1;

                if name.is_empty() || name == "." {
                    continue;
                }

                if name == ".." {
                    (nodeid, node_mode) = self.vfs.parent_of(nodeid)?;
                    rel_path.pop();
                    continue;
                }

                rel_path.push(name.to_owned());

                let parent = nodeid;
                match self.vfs.lookup(parent, name).await {
                    Ok((id, mode)) => {
                        nodeid = id;
                        node_mode = mode;
                    }
                    Err(err) => {
                        if err.errno == ENOENT && is_last_element && (flags & syscall::O_CREAT != 0)
                        {
                            let dir = flags & syscall::O_DIRECTORY != 0;
                            to_be_created = Some((name, dir));

                            break;
                        }
                        return Err(err);
                    }
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

        if to_be_created.is_none() && flags & syscall::O_CREAT != 0 && flags & syscall::O_EXCL != 0
        {
            return Err(Error::new(EEXIST));
        }

        if let Some((name, is_dir)) = to_be_created {
            // We broke out of the loop early, so `nodeid` is currently
            // the parent directory where the new file should be created.

            //TODO: clean
            let redox_mode = (flags & 0xFFFF) as u32;
            const S_IFREG: u32 = 0o100000;
            const S_IFDIR: u32 = 0o40000;
            let mode = if is_dir {
                redox_mode | S_IFDIR
            } else {
                redox_mode | S_IFREG
            };

            if is_dir {
                translate_redox_flags_to_fuse(flags, is_dir)?;
                (nodeid, node_mode) = self.vfs.mkdir(nodeid, name, mode).await?;
            } else {
                let fuse_flags = translate_redox_flags_to_fuse(flags, is_dir)?;
                let fd = self
                    .vfs
                    .create(nodeid, name, fuse_flags, mode, rel_path)
                    .await?;

                return Ok(OpenResult::ThisScheme {
                    number: fd,
                    flags: NewFdFlags::POSITIONED,
                });
            }
        }

        if flags & syscall::O_STAT != 0 {
            let fd = self.vfs.insert_stat_only(nodeid, rel_path)?;
            return Ok(OpenResult::ThisScheme {
                number: fd,
                flags: NewFdFlags::POSITIONED,
            });
        }

        let is_dir = node_mode == MODE_DIR;
        let fuse_flags = translate_redox_flags_to_fuse(flags, is_dir)?;
        let fd = self.vfs.open(nodeid, fuse_flags, is_dir, rel_path).await?;
        return Ok(OpenResult::ThisScheme {
            number: fd,
            flags: NewFdFlags::POSITIONED,
        });
    }

    async fn unlinkat(
        &mut self,
        fd: usize,
        path: &str,
        flags: usize,
        ctx: &CallerCtx,
    ) -> Result<()> {
        warn!("virtio-fsd: unlinkat is not supported on path {path}");
        Err(Error::new(EOPNOTSUPP))
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

        self.vfs.read(id, buf, offset).await
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

        self.vfs.write(id, buf, offset).await
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
        let path = self.vfs.get_path(id)?;

        FpathWriter::with(buf, self.scheme_name.as_ref(), |w| {
            w.push_str(path);
            Ok(())
        })
    }

    async fn frename(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        warn!("virtio-fsd: frename is not supported on id {id} and path {path}");
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, ctx: &CallerCtx) -> Result<()> {
        let attrs = self.vfs.getattr(id).await?;

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
        self.vfs.fsync(id).await
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
        let payload = self.vfs.readdir(id, opaque_offset).await?;

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
            let kind = DirentKind::try_from_raw(dir.typ as u8).unwrap_or(DirentKind::Unspecified);

            buf.entry(DirEntry {
                inode: dir.ino,
                next_opaque_id: dir.off,
                name,
                kind,
            })?;
        }
        Ok(buf)
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

// Helpers

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
