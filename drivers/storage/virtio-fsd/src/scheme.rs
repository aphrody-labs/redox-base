use std::io;

use log::warn;
use redox_scheme::scheme::{SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::FpathWriter;
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{EOPNOTSUPP, ESPIPE, Error, Result};
use syscall::schemev2::NewFdFlags;
use syscall::{
    EAGAIN, EBADMSG, EEXIST, EINTR, EINVAL, EISDIR, ELOOP, ENOENT, EWOULDBLOCK, EXDEV, EventFlags,
    MODE_DIR, MODE_SYMLINK, MapFlags, MunmapFlags, Stat, StatVfs, StdFsCallKind, StdFsCallMeta,
    TimeSpec,
};

use crate::fuse::{Caller, FuseTransporter};
use crate::vfs::Vfs;

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
        self.inner.vfs.close(id, Caller::NONE).await
    }

    pub async fn shutdown(&mut self) {
        self.inner.vfs.shutdown().await
    }

    pub async fn tick(&mut self) -> io::Result<()> {
        loop {
            let request = match self.inner.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    self.inner.vfs.shutdown().await;
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
        let caller = caller(ctx);

        #[cfg(feature = "lru-debug")]
        if let Some(number) = self.vfs.try_open_lru_log(fd, path, flags)? {
            return Ok(OpenResult::ThisScheme {
                number,
                flags: NewFdFlags::POSITIONED,
            });
        }

        let mut walk = self.vfs.walk_path(fd, path, flags, caller).await?;

        let to_be_created = if walk.last_to_be_created {
            let dir = flags & syscall::O_DIRECTORY != 0;
            Some((walk.rel_path.last().unwrap().clone(), dir))
        } else {
            None
        };

        let o_stat = flags & syscall::O_STAT != 0;
        let o_symlink = flags & syscall::O_SYMLINK != 0;
        let o_nofollow = flags & syscall::O_NOFOLLOW != 0;

        if o_symlink && flags & syscall::O_DIRECTORY != 0 {
            return Err(Error::new(EINVAL));
        }

        // ln -s does not send O_EXCL. If the name already exists, fail here
        // rather than opening the link and erroring on the later write.
        if o_symlink && flags & syscall::O_CREAT != 0 && to_be_created.is_none() {
            return Err(Error::new(EEXIST));
        }

        // O_SYMLINK opens the link itself (how Redox implements readlink).
        // O_STAT is lstat: metadata of the link, do not follow. Otherwise follow.
        if walk.node_mode == MODE_SYMLINK && !o_stat && !o_symlink {
            if o_nofollow {
                return Err(Error::new(ELOOP));
            }
            walk = self.vfs.follow_chain(walk, caller).await?;
        }

        if o_symlink && to_be_created.is_none() && walk.node_mode != MODE_SYMLINK {
            return Err(Error::new(EINVAL));
        }

        if to_be_created.is_none() && flags & syscall::O_CREAT != 0 && flags & syscall::O_EXCL != 0
        {
            return Err(Error::new(EEXIST));
        }

        if let Some((name, is_dir)) = to_be_created {
            // We broke out of the loop early, so `walk.nodeid` is currently
            // the parent directory where the new file should be created.

            if o_symlink {
                let fd = self
                    .vfs
                    .pre_create_symlink(walk.nodeid, name, walk.rel_path)?;
                return Ok(OpenResult::ThisScheme {
                    number: fd,
                    flags: NewFdFlags::POSITIONED,
                });
            }

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
                let accmode = flags & syscall::O_ACCMODE;
                if accmode == syscall::O_WRONLY || accmode == syscall::O_RDWR {
                    return Err(Error::new(EISDIR));
                }
                (walk.nodeid, walk.node_mode) =
                    self.vfs.mkdir(walk.nodeid, &name, mode, caller).await?;
            } else {
                let fd = self
                    .vfs
                    .create(walk.nodeid, &name, flags, mode, walk.rel_path, caller)
                    .await?;

                return Ok(OpenResult::ThisScheme {
                    number: fd,
                    flags: NewFdFlags::POSITIONED,
                });
            }
        }

        if o_stat {
            let fd = self.vfs.insert_stat_only(walk.nodeid, walk.rel_path)?;
            return Ok(OpenResult::ThisScheme {
                number: fd,
                flags: NewFdFlags::POSITIONED,
            });
        }

        if o_symlink {
            let target = self.vfs.readlink(walk.nodeid, caller).await?;
            let fd = self
                .vfs
                .insert_symlink(walk.nodeid, walk.rel_path, target)?;
            return Ok(OpenResult::ThisScheme {
                number: fd,
                flags: NewFdFlags::POSITIONED,
            });
        }

        let is_dir = walk.node_mode == MODE_DIR;
        if is_dir && flags & syscall::O_DIRECTORY == 0 {
            return Err(Error::new(EISDIR));
        }
        let fd = self
            .vfs
            .open(walk.nodeid, flags, is_dir, walk.rel_path, caller)
            .await?;
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
        let caller = caller(ctx);
        // relibc canonicalizes first, unlinkat never receives "." or ".."

        let walk = self.vfs.walk_path(fd, path, 0, caller).await?;
        if walk.last_to_be_created {
            return Err(Error::new(ENOENT));
        }

        let is_dir = flags & syscall::AT_REMOVEDIR == syscall::AT_REMOVEDIR;
        Ok(self.vfs.rm(walk, is_dir, caller).await?)
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
        self.vfs.read(id, buf, offset, caller(ctx)).await
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        self.vfs.write(id, buf, offset, caller(ctx)).await
    }

    async fn fsize(&mut self, id: usize, ctx: &CallerCtx) -> Result<u64> {
        warn!("virtio-fsd: fsize is not supported on id {id}");
        Err(Error::new(ESPIPE))
    }

    async fn fchmod(&mut self, id: usize, new_mode: u16, ctx: &CallerCtx) -> Result<()> {
        self.vfs.fchmod(id, new_mode, caller(ctx)).await
    }

    async fn fchown(
        &mut self,
        id: usize,
        new_uid: u32,
        new_gid: u32,
        ctx: &CallerCtx,
    ) -> Result<()> {
        self.vfs.fchown(id, new_uid, new_gid, caller(ctx)).await
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
        let dest = get_dest(&self.scheme_name, path)?;
        self.vfs.link(id, &dest, caller(ctx)).await?;
        Ok(0)
    }

    async fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let path = self.vfs.get_path(id)?;

        FpathWriter::with(buf, self.scheme_name.as_ref(), |w| {
            w.push_str(path);
            Ok(())
        })
    }

    async fn frename(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        let dest = get_dest(&self.scheme_name, path)?;
        self.vfs.rename(id, &dest, caller(ctx)).await?;
        Ok(0)
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, ctx: &CallerCtx) -> Result<()> {
        let attrs = self.vfs.getattr(id, caller(ctx)).await?;

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
        self.vfs.fsync(id, caller(ctx)).await
    }

    async fn ftruncate(&mut self, id: usize, len: u64, ctx: &CallerCtx) -> Result<()> {
        self.vfs.ftruncate(id, len, caller(ctx)).await
    }

    async fn futimens(&mut self, id: usize, times: &[TimeSpec], ctx: &CallerCtx) -> Result<()> {
        self.vfs.futimens(id, times, caller(ctx)).await
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
        let payload = self.vfs.readdir(id, opaque_offset, Caller::NONE).await?;

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

fn caller(ctx: &CallerCtx) -> Caller {
    Caller {
        uid: ctx.uid,
        gid: ctx.gid,
        pid: ctx.pid as u32,
    }
}

// Relibc `flink` and `frename` send path as `/scheme/<scheme_name>/…`
fn get_dest(scheme: &str, path: &str) -> Result<String> {
    if let Some(after) = path.strip_prefix("/scheme/") {
        let (name, rest) = after.split_once('/').unwrap_or((after, ""));
        if name != scheme {
            return Err(Error::new(EXDEV));
        }
        if rest.is_empty() {
            return Ok("/".to_owned());
        }
        return Ok(format!("/{rest}"));
    }
    if path.starts_with('/') {
        Ok(path.to_owned())
    } else {
        Ok(format!("/{path}"))
    }
}
