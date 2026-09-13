use std::io;
use std::sync::Arc;

use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;
use log::warn;
use redox_scheme::scheme::{SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::FpathWriter;
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{EOPNOTSUPP, Error, Result};
use syscall::schemev2::NewFdFlags;
use syscall::{
    EAGAIN, EBADMSG, EEXIST, EINTR, EINVAL, EISDIR, ELOOP, ENOENT, EPERM, EWOULDBLOCK, EXDEV,
    EventFlags, F_GETFL, F_SETFL, MODE_DIR, MODE_FILE, MODE_PERM, MODE_SYMLINK, MapFlags,
    MunmapFlags, Stat, StatVfs, StdFsCallKind, StdFsCallMeta, TimeSpec,
};

use crate::fuse::{Caller, FuseTransporter};
use crate::vfs::Vfs;

pub struct VirtioFsScheme<T: FuseTransporter> {
    socket: Arc<Socket>,
    vfs: Arc<Vfs<T>>,
    scheme_name: Arc<str>,
}

impl<T: FuseTransporter> Clone for VirtioFsScheme<T> {
    fn clone(&self) -> Self {
        Self {
            socket: Arc::clone(&self.socket),
            vfs: Arc::clone(&self.vfs),
            scheme_name: Arc::clone(&self.scheme_name),
        }
    }
}

impl<T: FuseTransporter + 'static> VirtioFsScheme<T> {
    pub fn new(socket: Arc<Socket>, vfs: Vfs<T>) -> Self {
        Self {
            vfs: Arc::new(vfs),
            socket,
            scheme_name: Arc::from("virtio-fsd"),
        }
    }

    pub async fn shutdown(&mut self) {
        self.vfs.shutdown().await
    }

    pub fn tick(&mut self) -> io::Result<()> {
        // We interleave requests on the same thread, then `pool.run()`
        // once EAGAIN is encountered.
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let mut closes = Vec::new();

        loop {
            let request = match self.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    pool.run();
                    futures::executor::block_on(self.vfs.shutdown());
                    std::process::exit(0);
                }
                Err(error) if error.errno == EWOULDBLOCK || error.errno == EAGAIN => break,
                Err(err) if err.errno == EINTR => continue,
                Err(err) => return Err(err.into()),
            };

            match request.kind() {
                RequestKind::Call(call_request) => {
                    let mut scheme = self.clone();
                    spawner
                        .spawn_local(async move {
                            let mut state = SchemeState::new();
                            let response = call_request.handle_async(&mut scheme, &mut state).await;
                            if let Err(err) = scheme
                                .socket
                                .write_response(response, SignalBehavior::Restart)
                            {
                                warn!("virtio-fsd: write_response: {err}");
                            }
                        })
                        .expect("virtio-fsd: failed to spawn scheme call");
                }
                RequestKind::SendFd(request) => {
                    let _ = self.socket.write_response(
                        Response::err(EOPNOTSUPP, request),
                        SignalBehavior::Restart,
                    );
                }
                RequestKind::RecvFd(request) => {
                    let _ = self.socket.write_response(
                        Response::err(EOPNOTSUPP, request),
                        SignalBehavior::Restart,
                    );
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {}
                RequestKind::OnClose { id } => closes.push(id),
                RequestKind::Cancellation(_cancellation_request) => {}
                RequestKind::OnDetach { .. } => {}
            }
        }

        pool.run();
        for id in closes {
            futures::executor::block_on(self.vfs.close(id, Caller::NONE));
        }
        Ok(())
    }
}

impl<T: FuseTransporter + 'static> SchemeAsync for VirtioFsScheme<T> {
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
        let flags = flags | fcntl_flags as usize;
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
            if o_symlink {
                let fd = self
                    .vfs
                    .pre_create_symlink(walk.nodeid, name, walk.rel_path)?;
                return Ok(OpenResult::ThisScheme {
                    number: fd,
                    flags: NewFdFlags::POSITIONED,
                });
            }

            let perm = flags as u32 & u32::from(MODE_PERM);
            if is_dir {
                let accmode = flags & syscall::O_ACCMODE;
                if accmode == syscall::O_WRONLY || accmode == syscall::O_RDWR {
                    return Err(Error::new(EISDIR));
                }
                (walk.nodeid, walk.node_mode) = self
                    .vfs
                    .mkdir(walk.nodeid, &name, perm | u32::from(MODE_DIR), caller)
                    .await?;
            } else {
                let fd = self
                    .vfs
                    .create(
                        walk.nodeid,
                        &name,
                        flags,
                        perm | u32::from(MODE_FILE),
                        walk.rel_path,
                        caller,
                    )
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

    // Ordinary dup/dup2 is done in the kernel and we never see it. This is only
    // for schemes that treat dup(fd, "some-name") as a special open, which we
    // don't. redoxfs skips it too.
    async fn dup(&mut self, _old_id: usize, _buf: &[u8], _ctx: &CallerCtx) -> Result<OpenResult> {
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
        self.vfs
            .read(id, buf, offset, fcntl_flags, caller(ctx))
            .await
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        self.vfs
            .write(id, buf, offset, fcntl_flags, caller(ctx))
            .await
    }

    async fn fsize(&mut self, id: usize, ctx: &CallerCtx) -> Result<u64> {
        Ok(self.vfs.getattr(id, caller(ctx)).await?.size)
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

    async fn fcntl(
        &mut self,
        id: usize,
        cmd: usize,
        _arg: usize,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        // F_GETFL/F_SETFL: kernel keeps the bits and passes them back as
        // fcntl_flags on read/write. We only have to agree. F_GETFD/F_SETFD
        // and F_DUPFD never reach the scheme.
        self.vfs.has_fd(id)?;
        match cmd {
            F_GETFL | F_SETFL => Ok(0),
            _ => Err(Error::new(EINVAL)),
        }
    }

    async fn fevent(
        &mut self,
        id: usize,
        _flags: EventFlags,
        _ctx: &CallerCtx,
    ) -> Result<EventFlags> {
        // files here are always ready, same as redoxfs.
        self.vfs.has_fd(id)?;
        Err(Error::new(EPERM))
    }

    async fn flink(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        let dest = get_dest(self.scheme_name.as_ref(), path)?;
        self.vfs.link(id, &dest, caller(ctx)).await?;
        Ok(0)
    }

    async fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let path = self.vfs.get_path(id)?;

        FpathWriter::with(buf, self.scheme_name.as_ref(), |w| {
            w.push_str(&path);
            Ok(())
        })
    }

    async fn frename(&mut self, id: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        let dest = get_dest(self.scheme_name.as_ref(), path)?;
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
        let k = self.vfs.statfs(id, caller(ctx)).await?;
        let bsize = if k.bsize != 0 { k.bsize } else { k.frsize };

        *stat = StatVfs {
            f_bsize: bsize,
            f_blocks: k.blocks,
            f_bfree: k.bfree,
            f_bavail: k.bavail,
        };

        Ok(())
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
        _id: usize,
        _payload: &mut [u8],
        _metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    async fn std_fs_call(
        &mut self,
        _id: usize,
        _kind: StdFsCallKind,
        _payload: &mut [u8],
        _metadata: StdFsCallMeta,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
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

    // Mapping file contents needs DAX, which we aren't doing yet.
    async fn mmap_prep(
        &mut self,
        _id: usize,
        _offset: u64,
        _size: usize,
        _flags: MapFlags,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    // Only needed once mmap_prep actually maps something.
    async fn munmap(
        &mut self,
        _id: usize,
        _offset: u64,
        _size: usize,
        _flags: MunmapFlags,
        _ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    async fn on_recvfd(&mut self, _recvfd_request: &RecvFdRequest) -> Result<OpenResult> {
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
