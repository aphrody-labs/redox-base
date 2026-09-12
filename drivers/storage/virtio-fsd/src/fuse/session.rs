use std::ffi::CString;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use fuse_abi::linux::{
    FUSE_ATOMIC_O_TRUNC, FUSE_EXPORT_SUPPORT, FUSE_GETATTR_FH, FUSE_KERNEL_MINOR_VERSION,
    FUSE_KERNEL_VERSION, FUSE_MAX_PAGES, FUSE_NO_EXPORT_SUPPORT, FUSE_ROOT_ID, fuse_attr_out,
    fuse_create_in, fuse_entry_out, fuse_forget_in, fuse_fsync_in, fuse_getattr_in, fuse_in_header,
    fuse_init_in, fuse_init_out, fuse_link_in, fuse_mkdir_in, fuse_opcode, fuse_open_in,
    fuse_open_out, fuse_out_header, fuse_read_in, fuse_release_in, fuse_rename_in, fuse_setattr_in,
    fuse_statfs_out, fuse_write_in, fuse_write_out,
};
use log::{info, warn};
use syscall::error::{EAGAIN, EIO, ENODEV, ENOMEM, Error};
use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::fuse::protocol::*;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Memory exhausted.")]
    OutOfMemory,
    #[error("Device is offline.")]
    DeviceOffline, // TODO: virtio-core must be patched to return this error when the device goes offline, instead of panicking or blocking indefinitely.
    #[error("Device is busy processing other requests.")]
    Busy, // TODO: virtio-core must be patched to return this error when the virtqueue is full, instead of blocking until space is available.
    #[error("Generic transport error")]
    Generic,
}

#[derive(Debug, Error)]
pub enum FuseError {
    #[error("Transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("Host rejected request with POSIX code: {0}")]
    Host(i32),
    #[error("Received malformed payload from transport")]
    MalformedPayload,
}

impl FuseError {
    pub fn from_fuse_header(error: i32) -> Result<(), Self> {
        if error == 0 {
            Ok(())
        } else {
            Err(Self::Host(error.wrapping_neg()))
        }
    }

    pub fn into_syscall(self, op: &str) -> Error {
        if !matches!(self, Self::Host(_)) {
            warn!("virtio-fsd: {op}: {self}");
        }
        match self {
            Self::Host(errno) => Error::new(errno),
            Self::Transport(TransportError::OutOfMemory) => Error::new(ENOMEM),
            Self::Transport(TransportError::Busy) => Error::new(EAGAIN),
            Self::Transport(TransportError::DeviceOffline) => Error::new(ENODEV),
            Self::Transport(TransportError::Generic) | Self::MalformedPayload => Error::new(EIO),
        }
    }
}

pub struct FuseRequest<I> {
    pub fuse_in_header: fuse_in_header,
    pub fuse_in: Option<I>,
}

pub enum OutPayload<O> {
    None,
    Struct(O),
    Size(usize),
}

pub struct FuseResponse<O> {
    pub fuse_out_header: fuse_out_header,
    pub payload: OutPayload<O>,
}

impl<O> FuseResponse<O> {
    pub fn into_struct(self) -> Result<O, FuseError> {
        FuseError::from_fuse_header(self.fuse_out_header.error)?;
        match self.payload {
            OutPayload::Struct(s) => Ok(s),
            _ => Err(FuseError::MalformedPayload),
        }
    }

    pub fn into_unit(self) -> Result<(), FuseError> {
        FuseError::from_fuse_header(self.fuse_out_header.error)?;
        match self.payload {
            OutPayload::None => Ok(()),
            _ => Err(FuseError::MalformedPayload),
        }
    }

    pub fn into_size(self) -> Result<usize, FuseError> {
        FuseError::from_fuse_header(self.fuse_out_header.error)?;
        match self.payload {
            OutPayload::Size(n) => Ok(n),
            _ => Err(FuseError::MalformedPayload),
        }
    }
}

/// How the transporter should handle the FUSE data payload.
pub enum AllocStrategy<'a> {
    /// When everything fits in PAGE_SIZE and response has a header and a payload.
    DmaPool,
    /// When everything fits in PAGE_SIZE and response only has a header.
    DmaPoolHeaderOnly,
    /// Massive transmit (e.g. `FUSE_WRITE`).
    /// `data` will directly get copied to DMA memory.
    Send { data: &'a [u8] },
    /// Massive Receive (e.g. `FUSE_READ`).
    /// Data will directly get copied to `buf` from DMA memory.
    Recv { buf: &'a mut [u8] },
}

pub trait FuseTransporter {
    async fn send<I, O>(
        &self,
        request: FuseRequest<I>,
        strategy: AllocStrategy<'_>,
    ) -> Result<FuseResponse<O>, TransportError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable;

    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), TransportError>
    where
        I: IntoBytes + Immutable;
}

/// Identity for `fuse_in_header`.
/// `NONE` (zeros) when the scheme call has no `CallerCtx` (INIT, OnClose, getdents, unmount).
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl Caller {
    pub const NONE: Self = Self {
        uid: 0,
        gid: 0,
        pid: 0,
    };
}

pub struct FuseSession<T: FuseTransporter> {
    next_unique: AtomicU64,
    init: fuse_init_out,
    transporter: T,
}

impl<T> FuseSession<T>
where
    T: FuseTransporter,
{
    /// Flags we put on FUSE_INIT. The host replies with the intersection.
    const CLIENT_INIT_FLAGS: u64 = FUSE_MAX_PAGES as u64 | FUSE_ATOMIC_O_TRUNC as u64;

    /// Initializes the FUSE session by sending a FUSE_INIT request to the device.
    #[must_use = "The function returns a future that must be awaited to ensure the sent request is completed."]
    pub async fn new(transporter: T) -> Result<Self, FuseError> {
        let mut session = Self {
            transporter,
            init: fuse_init_out::default(),
            next_unique: AtomicU64::new(1),
        };

        let fuse_in_header = session.in_header(
            fuse_opcode::FUSE_INIT.into(),
            FUSE_ROOT_ID,
            mem::size_of::<fuse_init_in>(),
            None,
            Caller::NONE,
        );

        let flags = Self::CLIENT_INIT_FLAGS;
        let init_in = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 1 << 20, // 1 MiB
            flags: flags as u32,
            flags2: (flags >> 32) as u32,
            unused: [0; 11],
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(init_in),
        };

        let init_out = session
            .transporter
            .send::<_, fuse_init_out>(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()?;

        if init_out.major != FUSE_KERNEL_VERSION {
            warn!(
                "virtio-fsd: FUSE major {} (client offered {})",
                init_out.major, FUSE_KERNEL_VERSION
            );
        }

        session.init = init_out;

        warn!(
            "virtio-fsd: FUSE INIT succeeded:
            - protocol {}.{}
            - max_write={}
            - max_readahead={}
            - max_background={}
            - time_gran={}
            - flags={:#x}
            - congestion_threshold={}
            - max_pages={}
            - map_alignment={}
            - flags2={:#x}
            - max_stack_depth={}",
            session.init.major,
            session.init.minor,
            session.init.max_write,
            session.init.max_readahead,
            session.init.max_background,
            session.init.time_gran,
            session.init.flags,
            session.init.congestion_threshold,
            session.init.max_pages,
            session.init.map_alignment,
            session.init.flags2,
            session.init.max_stack_depth,
        );

        let use_local_parent_resolution =
            session.capable(FUSE_NO_EXPORT_SUPPORT) || !session.capable(FUSE_EXPORT_SUPPORT);

        // currently we use parent_map for both scenarios, as
        // virtiofsd does not support `FUSE_NO_EXPORT_SUPPORT` flag
        if use_local_parent_resolution {
            info!(
                "Host disabled export support (or is old). lookup('..') must be resolved locally."
            );
        } else {
            info!("Host enabled full export support. lookup('..') can be forwarded to host.");
        }

        Ok(session)
    }

    /// Whether this `FUSE_INIT` flag was negotiated (`flags` / `flags2`).
    pub fn capable(&self, cap: impl Into<u64>) -> bool {
        (self.init.flags as u64 | (self.init.flags2 as u64) << 32) & cap.into() != 0
    }

    /// `extra_len` is the size of the in-payload after this header (`len` = header + extra_len).
    /// `unique` is allocated from `next_unique` when `None`. FUSE_FORGET must send `Some(0)`.
    fn in_header(
        &self,
        opcode: u32,
        nodeid: u64,
        extra_len: usize,
        unique: Option<u64>,
        caller: Caller,
    ) -> fuse_in_header {
        fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + extra_len) as u32,
            opcode,
            unique: unique.unwrap_or_else(|| self.next_unique.fetch_add(1, Ordering::SeqCst)),
            nodeid,
            uid: caller.uid,
            gid: caller.gid,
            pid: caller.pid,
            total_extlen: 0,
            padding: 0,
        }
    }

    /// Handles both `open` and `opendir` opcodes based on `is_dir`.
    pub async fn open(
        &self,
        nodeid: u64,
        flags: u32,
        is_dir: bool,
        caller: Caller,
    ) -> Result<fuse_open_out, FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_OPENDIR.into()
        } else {
            fuse_opcode::FUSE_OPEN.into()
        };
        let fuse_in_header =
            self.in_header(opcode, nodeid, mem::size_of::<fuse_open_in>(), None, caller);

        let fuse_in = fuse_open_in {
            flags,
            open_flags: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()
    }

    pub async fn getattr(
        &self,
        nodeid: u64,
        fh: Option<u64>,
        caller: Caller,
    ) -> Result<fuse_attr_out, FuseError> {
        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_GETATTR.into(),
            nodeid,
            mem::size_of::<fuse_getattr_in>(),
            None,
            caller,
        );

        let fuse_in = fuse_getattr_in {
            getattr_flags: if fh.is_some() { FUSE_GETATTR_FH } else { 0 },
            dummy: 0,
            fh: if fh.is_some() { fh.unwrap() } else { 0 },
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()
    }

    pub async fn statfs(&self, nodeid: u64, caller: Caller) -> Result<fuse_statfs_out, FuseError> {
        let fuse_in_header =
            self.in_header(fuse_opcode::FUSE_STATFS.into(), nodeid, 0, None, caller);

        let request: FuseRequest<()> = FuseRequest {
            fuse_in_header,
            fuse_in: None,
        };

        self.transporter
            .send(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()
    }

    pub async fn setattr(
        &self,
        nodeid: u64,
        fuse_in: fuse_setattr_in,
        caller: Caller,
    ) -> Result<fuse_attr_out, FuseError> {
        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_SETATTR.into(),
            nodeid,
            mem::size_of::<fuse_setattr_in>(),
            None,
            caller,
        );

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()
    }

    pub async fn readdir(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        caller: Caller,
    ) -> Result<FuseReaddirOut, FuseError> {
        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_READDIR.into(),
            nodeid,
            mem::size_of::<fuse_read_in>(),
            None,
            caller,
        );

        let fuse_in = fuse_read_in {
            fh,
            offset,
            size: FUSE_READDIR_BUFFER_SIZE,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        let mut buf = vec![0u8; FUSE_READDIR_BUFFER_SIZE as usize];

        let n = self
            .transporter
            .send::<_, ()>(request, AllocStrategy::Recv { buf: &mut buf })
            .await?
            .into_size()?;

        buf.truncate(n);
        Ok(FuseReaddirOut::new(buf))
    }

    pub async fn lookup(
        &self,
        parent_nodeid: u64,
        name: CString,
        caller: Caller,
    ) -> Result<fuse_entry_out, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let mut name_buf = [0u8; NAME_MAX];
        name_buf[..name.len()].copy_from_slice(&name);

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_LOOKUP.into(),
            parent_nodeid,
            name_len,
            None,
            caller,
        );

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(name_buf),
        };

        self.transporter
            .send(request, AllocStrategy::DmaPool)
            .await?
            .into_struct()
    }

    pub async fn read(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
        read_flags: u32,
        flags: u32,
        caller: Caller,
    ) -> Result<usize, FuseError> {
        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_READ.into(),
            nodeid,
            mem::size_of::<fuse_read_in>(),
            None,
            caller,
        );

        // TODO: make sure size is <= max_writeself.init.max_pages as u32 * linux_abi::PAGE_SIZE - otherwise, chunk the data
        // let max_size = core::cmp::min(buf.len(), u32::MAX as usize) as u32;
        // let size = core::cmp::min(max_size, self.init.max_pages as u32 * linux_abi::PAGE_SIZE);

        let fuse_in = fuse_read_in {
            fh,
            offset,
            size: buf.len() as u32,
            read_flags,
            lock_owner: 0,
            flags,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send::<_, ()>(request, AllocStrategy::Recv { buf })
            .await?
            .into_size()
    }

    pub async fn write(
        &self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: u32,
        caller: Caller,
    ) -> Result<fuse_write_out, FuseError> {
        let size = data.len();

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_WRITE.into(),
            nodeid,
            mem::size_of::<fuse_write_in>() + size,
            None,
            caller,
        );

        // TODO: make sure size is <= max_write - otherwise, chunk the data
        // let request_size = core::cmp::min(size, self.init.max_write as usize);

        let fuse_in = fuse_write_in {
            fh,
            offset,
            size: core::cmp::min(size, u32::MAX as usize) as u32,
            write_flags,
            lock_owner: 0,
            flags,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::Send { data })
            .await?
            .into_struct()
    }

    pub async fn create(
        &self,
        parent_nodeid: u64,
        name: CString,
        flags: u32,
        mode: u32,
        caller: Caller,
    ) -> Result<FuseCreateOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_CREATE.into(),
            parent_nodeid,
            mem::size_of::<fuse_create_in>() + name_len,
            None,
            caller,
        );

        let fuse_in = fuse_create_in {
            flags,
            mode,
            umask: 0,
            open_flags: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::Send { data: name })
            .await?
            .into_struct()
    }

    pub async fn fsync(
        &self,
        nodeid: u64,
        fh: u64,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_FSYNCDIR.into()
        } else {
            fuse_opcode::FUSE_FSYNC.into()
        };
        let fuse_in_header = self.in_header(
            opcode,
            nodeid,
            mem::size_of::<fuse_fsync_in>(),
            None,
            caller,
        );

        let fuse_in = fuse_fsync_in {
            fh,
            fsync_flags: 0,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send::<_, ()>(request, AllocStrategy::DmaPoolHeaderOnly)
            .await?
            .into_unit()
    }

    pub async fn release(
        &self,
        nodeid: u64,
        fh: u64,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_RELEASEDIR.into()
        } else {
            fuse_opcode::FUSE_RELEASE.into()
        };
        let fuse_in_header = self.in_header(
            opcode,
            nodeid,
            mem::size_of::<fuse_release_in>(),
            None,
            caller,
        );

        let fuse_in = fuse_release_in {
            fh,
            flags: 0,
            release_flags: 0,
            lock_owner: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send::<_, ()>(request, AllocStrategy::DmaPoolHeaderOnly)
            .await?
            .into_unit()
    }

    pub async fn forget(&self, nodeid: u64, nlookup: u64, caller: Caller) -> Result<(), FuseError> {
        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_FORGET.into(),
            nodeid,
            mem::size_of::<fuse_forget_in>(),
            Some(0),
            caller,
        );

        let fuse_in = fuse_forget_in { nlookup };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter.send_hiprio_noreply(request).await?;

        Ok(())
    }

    pub async fn mkdir(
        &self,
        parent_nodeid: u64,
        name: CString,
        mode: u32,
        caller: Caller,
    ) -> Result<fuse_entry_out, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_MKDIR.into(),
            parent_nodeid,
            mem::size_of::<fuse_mkdir_in>() + name_len,
            None,
            caller,
        );

        let fuse_in = fuse_mkdir_in { mode, umask: 0 };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::Send { data: name })
            .await?
            .into_struct()
    }

    pub async fn rm(
        &self,
        parent_nodeid: u64,
        name: CString,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let mut name_buf = [0u8; NAME_MAX];
        name_buf[..name.len()].copy_from_slice(&name);

        let opcode = if is_dir {
            fuse_opcode::FUSE_RMDIR.into()
        } else {
            fuse_opcode::FUSE_UNLINK.into()
        };
        let fuse_in_header = self.in_header(opcode, parent_nodeid, name_len, None, caller);

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(name_buf),
        };

        self.transporter
            .send::<_, ()>(request, AllocStrategy::DmaPoolHeaderOnly)
            .await?
            .into_unit()
    }

    pub async fn readlink(&self, nodeid: u64, caller: Caller) -> Result<String, FuseError> {
        let fuse_in_header =
            self.in_header(fuse_opcode::FUSE_READLINK.into(), nodeid, 0, None, caller);

        let request: FuseRequest<()> = FuseRequest {
            fuse_in_header,
            fuse_in: None,
        };

        let mut buf = vec![0u8; FUSE_READLINK_MAX];

        let n = self
            .transporter
            .send::<_, ()>(request, AllocStrategy::Recv { buf: &mut buf })
            .await?
            .into_size()?;

        if n > FUSE_READLINK_MAX {
            warn!("virtio-fsd: readlink: path length is larger than FUSE_READLINK_MAX");
            return Err(FuseError::MalformedPayload);
        }

        buf.truncate(n);
        while buf.last() == Some(&0) {
            buf.pop();
        }

        String::from_utf8(buf).map_err(|e| {
            warn!("virtio-fsd: readlink: path is not a valid utf8: {e:?}");
            FuseError::MalformedPayload
        })
    }

    pub async fn symlink(
        &self,
        parent_nodeid: u64,
        name: &CString,
        path: &CString,
        caller: Caller,
    ) -> Result<fuse_entry_out, FuseError> {
        let name = name.as_bytes_with_nul();
        let path = path.as_bytes_with_nul();

        let name_len = name.len();
        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let path_len = path.len();
        if name_len + path_len > FUSE_SYMLINK_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_SYMLINK.into(),
            parent_nodeid,
            name_len + path_len,
            None,
            caller,
        );

        let mut data = [0u8; FUSE_SYMLINK_MAX];

        data[..name_len].copy_from_slice(name);
        data[name_len..name_len + path_len].copy_from_slice(path);

        let request: FuseRequest<()> = FuseRequest {
            fuse_in_header,
            fuse_in: None,
        };

        self.transporter
            .send(
                request,
                AllocStrategy::Send {
                    data: &data[..name_len + path_len],
                },
            )
            .await?
            .into_struct()
    }

    pub async fn rename(
        &self,
        old_parent: u64,
        old_name: CString,
        new_parent: u64,
        new_name: CString,
        caller: Caller,
    ) -> Result<(), FuseError> {
        const RENAME_IN_LEN: usize = mem::size_of::<fuse_rename_in>();

        let old = old_name.as_bytes_with_nul();
        let new = new_name.as_bytes_with_nul();
        if old.len() > NAME_MAX || new.len() > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let extra = RENAME_IN_LEN + old.len() + new.len();
        let fuse_in_header = self.in_header(
            // switch to FUSE_RENAME2 if scheme starts accepting flags
            fuse_opcode::FUSE_RENAME.into(),
            old_parent,
            extra,
            None,
            caller,
        );

        let rename_in = fuse_rename_in { newdir: new_parent };

        let mut data = [0u8; RENAME_IN_LEN + NAME_MAX * 2];

        data[..RENAME_IN_LEN].copy_from_slice(rename_in.as_bytes());
        data[RENAME_IN_LEN..RENAME_IN_LEN + old.len()].copy_from_slice(old);
        data[RENAME_IN_LEN + old.len()..RENAME_IN_LEN + old.len() + new.len()].copy_from_slice(new);

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(data),
        };

        self.transporter
            .send::<_, ()>(request, AllocStrategy::DmaPoolHeaderOnly)
            .await?
            .into_unit()
    }

    pub async fn link(
        &self,
        new_parent: u64,
        old_nodeid: u64,
        name: CString,
        caller: Caller,
    ) -> Result<fuse_entry_out, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();
        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in_header = self.in_header(
            fuse_opcode::FUSE_LINK.into(),
            new_parent,
            mem::size_of::<fuse_link_in>() + name_len,
            None,
            caller,
        );

        let fuse_in = fuse_link_in {
            oldnodeid: old_nodeid,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        self.transporter
            .send(request, AllocStrategy::Send { data: name })
            .await?
            .into_struct()
    }
}
