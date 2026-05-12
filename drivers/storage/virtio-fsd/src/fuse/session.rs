use fuse_abi::linux::{
    FUSE_EXPORT_SUPPORT, FUSE_GETATTR_FH, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
    FUSE_MAX_PAGES, FUSE_NO_EXPORT_SUPPORT, FUSE_ROOT_ID, fuse_attr_out, fuse_create_in,
    fuse_entry_out, fuse_fsync_in, fuse_getattr_in, fuse_in_header, fuse_init_in, fuse_init_out,
    fuse_opcode, fuse_open_in, fuse_open_out, fuse_out_header, fuse_read_in, fuse_write_in,
    fuse_write_out,
};
use log::{info, warn};
use std::{ffi::CString, mem, sync::atomic::AtomicU64};
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
    HostError(i32),
    #[error("Received malformed payload from transport")]
    MalformedPayload,
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
    pub payload_size: usize,
    pub payload: OutPayload<O>,
}

/// How the transporter should handle the FUSE data payload.
pub enum AllocStrategy<'a> {
    /// When everything fits in PAGE_SIZE (e.g., `FUSE_LOOKUP`, `FUSE_OPEN`)
    DmaPool { has_out_payload: bool },
    /// Massive transmit (e.g. `FUSE_WRITE`).
    /// `data` will directly get copied to DMA memory.
    Send { data: &'a [u8] },
    /// Massive Receive (e.g. `FUSE_READ`).
    /// Data will directly get copied to `buf` from DMA memory.
    Recv { buf: &'a mut [u8] },
}

pub trait FuseTransporter {
    async fn send_request<I, O>(
        &self,
        request: FuseRequest<I>,
        strategy: AllocStrategy<'_>,
    ) -> Result<FuseResponse<O>, TransportError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable;
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
    /// Initializes the FUSE session by sending a FUSE_INIT request to the device.
    ///
    /// # Panics
    /// This function panics if the fuse init request fails.
    #[must_use = "The function returns a future that must be awaited to ensure the sent request is completed."]
    pub async fn new(transporter: T) -> Self {
        let mut ret = Self {
            transporter,
            init: fuse_init_out::default(),
            next_unique: AtomicU64::new(0),
        };

        let unique = ret
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_init_in>()) as u32,
            opcode: fuse_opcode::FUSE_INIT.into(),
            unique,
            nodeid: FUSE_ROOT_ID,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let flags: u64 = FUSE_MAX_PAGES as u64;

        let init_in = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 1 << 20, // 1 MiB
            flags: flags as u32,
            flags2: (flags >> 32) as u32, // shift down the flags above bit 31 into flags2
            unused: [0; 11],
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(init_in),
        };

        let response = ret
            .transporter
            .send_request::<_, fuse_init_out>(
                request,
                AllocStrategy::DmaPool {
                    has_out_payload: true,
                },
            )
            .await;

        match response {
            Ok(response) => match response.fuse_out_header.error {
                0 => {
                    warn!(
                        "virtio-fsd: FUSE INIT response received ('{}' total bytes, '{}' payload)",
                        response.fuse_out_header.len, response.payload_size
                    );

                    ret.init = match response.payload {
                        OutPayload::Struct(s) => s,
                        _ => panic!("virtio-fsd: FUSE INIT response missing payload"),
                    };
                }
                err_code => panic!("virtio-fsd: FUSE INIT failed with errno {err_code}"),
            },
            Err(err) => panic!("virtio-fsd: FUSE INIT failed with {err}"),
        }

        warn!(
            "virtio-fsd: FUSE INIT succeeded:
                 - protocol {}.{}
                 - max_write={}
                 - max_readahead={}
                 - max_background={}
                 - time_gran={}
                 - flags={}
                 - congestion_threshold={}
                 - max_pages={}
                 - map_alignment={}
                 - flags2={}
                 - max_stack_depth={}",
            ret.init.major,
            ret.init.minor,
            ret.init.max_write,
            ret.init.max_readahead,
            ret.init.max_background,
            ret.init.time_gran,
            ret.init.flags,
            ret.init.congestion_threshold,
            ret.init.max_pages,
            ret.init.map_alignment,
            ret.init.flags2,
            ret.init.max_stack_depth,
        );

        // TODO: check response flags and change behaviour accordingly

        let init_flags = ret.init.flags as u64 | (ret.init.flags2 as u64) << 32;

        let use_local_parent_resolution = (init_flags & FUSE_NO_EXPORT_SUPPORT) != 0
            || (init_flags & FUSE_EXPORT_SUPPORT as u64) == 0;

        // currently we use parent_map for both scenarios, as
        // virtiofsd does not support `FUSE_NO_EXPORT_SUPPORT` flag
        if use_local_parent_resolution {
            info!(
                "Host disabled export support (or is old). lookup('..') must be resolved locally."
            );
        } else {
            info!("Host enabled full export support. lookup('..') can be forwarded to host.");
        }

        return ret;
    }

    /// Handles both `open` and `opendir` opcodes based on `is_dir`.
    pub async fn open(
        &mut self,
        nodeid: u64,
        flags: u32,
        is_dir: bool,
    ) -> Result<fuse_open_out, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let opcode = if is_dir {
            fuse_opcode::FUSE_OPENDIR.into()
        } else {
            fuse_opcode::FUSE_OPEN.into()
        };

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_open_in>()) as u32,
            opcode,
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let fuse_in = fuse_open_in {
            flags,
            open_flags: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        let response = self
            .transporter
            .send_request(
                request,
                AllocStrategy::DmaPool {
                    has_out_payload: true,
                },
            )
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Struct(s) => Ok(s),
                _ => {
                    let op = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };
                    warn!("virtio-fsd: {op} response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn getattr(
        &mut self,
        nodeid: u64,
        fh: Option<u64>,
    ) -> Result<fuse_attr_out, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_getattr_in>()) as u32,
            opcode: fuse_opcode::FUSE_GETATTR.into(),
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let fuse_in = fuse_getattr_in {
            getattr_flags: if fh.is_some() { FUSE_GETATTR_FH } else { 0 },
            dummy: 0,
            fh: if fh.is_some() { fh.unwrap() } else { 0 },
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        let response = self
            .transporter
            .send_request(
                request,
                AllocStrategy::DmaPool {
                    has_out_payload: true,
                },
            )
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Struct(s) => Ok(s),
                _ => {
                    warn!("virtio-fsd: FUSE_GETATTR response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn readdir(
        &mut self,
        nodeid: u64,
        fh: u64,
        offset: u64,
    ) -> Result<FuseReaddirOut, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_read_in>()) as u32,
            opcode: fuse_opcode::FUSE_READDIR.into(),
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

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

        let response = self
            .transporter
            .send_request::<_, ()>(request, AllocStrategy::Recv { buf: &mut buf })
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Size(s) => {
                    buf.truncate(s);
                    Ok(FuseReaddirOut::new(buf))
                }
                _ => {
                    warn!("virtio-fsd: FUSE_READDIR response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn lookup(
        &mut self,
        root_nodeid: u64,
        name: &str,
    ) -> Result<fuse_entry_out, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut name = name.as_bytes().to_vec();

        if name.len() > FUSE_LOOKUP_NAME_MAX {
            return Err(FuseError::HostError(syscall::ENAMETOOLONG as i32));
        }

        name.push(0);

        // TODO: This can be simplified, once send_request accepts a pre allocated dma buffer.
        let mut name_buf = [0u8; FUSE_LOOKUP_NAME_MAX + 1];
        name_buf[..name.len()].copy_from_slice(&name);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + name.len()) as u32,
            opcode: fuse_opcode::FUSE_LOOKUP.into(),
            unique,
            nodeid: root_nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(name_buf),
        };

        let response = self
            .transporter
            .send_request(
                request,
                AllocStrategy::DmaPool {
                    has_out_payload: true,
                },
            )
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Struct(s) => Ok(s),
                _ => {
                    warn!("virtio-fsd: FUSE_LOOKUP response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn read(
        &mut self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
        read_flags: u32,
        flags: u32,
    ) -> Result<usize, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_read_in>()) as u32,
            opcode: fuse_opcode::FUSE_READ.into(),
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

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

        let response = self
            .transporter
            .send_request::<_, ()>(request, AllocStrategy::Recv { buf })
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Size(s) => Ok(s),
                _ => {
                    warn!("virtio-fsd: FUSE_READ response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn write(
        &mut self,
        nodeid: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: u32,
    ) -> Result<fuse_write_out, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let size = data.len();

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_write_in>() + size) as u32,
            opcode: fuse_opcode::FUSE_WRITE.into(),
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

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

        let response = self
            .transporter
            .send_request(request, AllocStrategy::Send { data })
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Struct(s) => Ok(s),
                _ => {
                    warn!("virtio-fsd: FUSE_WRITE response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn create(
        &mut self,
        nodeid: u64,
        file_name: CString,
        flags: u32,
        mode: u32,
    ) -> Result<FuseCreateOut, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let data = file_name.as_bytes_with_nul();
        let size = data.len();

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_create_in>() + size)
                as u32,
            opcode: fuse_opcode::FUSE_CREATE.into(),
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

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

        let response = self
            .transporter
            .send_request(request, AllocStrategy::Send { data })
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::Struct(s) => Ok(s),
                _ => {
                    warn!("virtio-fsd: FUSE_CREATE response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn fsync(&mut self, nodeid: u64, fh: u64, is_dir: bool) -> Result<(), FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<fuse_fsync_in>()) as u32,
            opcode: if is_dir {
                fuse_opcode::FUSE_FSYNCDIR.into()
            } else {
                fuse_opcode::FUSE_FSYNC.into()
            },
            unique,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        };

        let fuse_in = fuse_fsync_in {
            fh,
            fsync_flags: 0,
            padding: 0,
        };

        let request = FuseRequest {
            fuse_in_header,
            fuse_in: Some(fuse_in),
        };

        let response = self
            .transporter
            .send_request::<_, ()>(
                request,
                AllocStrategy::DmaPool {
                    has_out_payload: false,
                },
            )
            .await?;

        match response.fuse_out_header.error {
            0 => match response.payload {
                OutPayload::None => Ok(()),
                _ => {
                    let op = if is_dir {
                        "FUSE_FSYNCDIR"
                    } else {
                        "FUSE_FSYNC"
                    };
                    warn!("virtio-fsd: {op} response was malformed");
                    Err(FuseError::MalformedPayload)
                }
            },
            err_code => Err(FuseError::HostError(err_code)),
        }
    }
}
