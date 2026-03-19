use fuse_abi::linux::{
    FUSE_GETATTR_FH, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_MAX_PAGES, FUSE_ROOT_ID,
    fuse_attr_out, fuse_entry_out, fuse_getattr_in, fuse_in_header, fuse_init_in, fuse_init_out,
    fuse_opcode, fuse_open_in, fuse_open_out, fuse_out_header, fuse_read_in,
};
use log::warn;
use std::{mem, sync::atomic::AtomicU64};
use thiserror::Error;

use crate::fuse::protocol::*;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Memory exhausted.")]
    OutOfMemory,
    #[error("Device is offline.")]
    DeviceOffline, // TODO: virtio-core must be patched to return this error when the device goes offline, instead pf panicking or blocking indefinitely.
    #[error("Device is busy processing other requests.")]
    Busy, // TODO: virtio-core must be patched to return this error when the virtqueue is full, instead of blocking until space is available.
}

#[derive(Debug, Error)]
pub enum FuseError {
    #[error("Transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("Host rejected request with POSIX code: {0}")]
    HostError(i32),
    #[error("Received malformed payload from transport")]
    MalformedPayload, // TODO: ?
}

pub struct FuseRequest<I, O> {
    // Device-Readable (Going to the Host)
    pub in_header: fuse_in_header,
    pub in_payload: Option<I>,
    // Device-Writable (Coming back from the Host)
    pub out_header: fuse_out_header,
    pub out_payload: Option<O>,
}

pub struct FuseResponse<O> {
    /// The length of the response written by the device, including the header and payload.
    /// This is used to determine how much of the out_payload is valid.
    pub written_len: u32,
    pub out_header: fuse_out_header,
    pub out_payload: Option<O>,
}

pub trait FuseTransporter {
    async fn send_request<I, O>(
        &self,
        request: FuseRequest<I, O>,
    ) -> Result<FuseResponse<O>, TransportError>
    where
        I: Sized,
        O: Sized + Clone;
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

        let in_header = fuse_in_header {
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

        let flags = FUSE_MAX_PAGES;

        let init_in = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 1 << 20, // 1 MiB
            flags: flags as u32,
            flags2: (flags >> 32) as u32, // shift down the flags above bit 31 into flags2
            unused: [0; 11],
        };

        let request = FuseRequest {
            in_header: in_header,
            in_payload: Some(init_in),
            out_header: fuse_out_header::default(),
            out_payload: Some(fuse_init_out::default()),
        };

        let response = ret.transporter.send_request(request).await;

        match response {
            Ok(response) => match response.out_header.error {
                0 => {
                    warn!(
                        "virtio-fsd: FUSE INIT response received ({} bytes written by device)",
                        response.written_len
                    );

                    ret.init = response
                        .out_payload
                        .expect("virtio-fsd: FUSE INIT response missing payload");
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

        return ret;
    }

    /// Handles both `open` and `opendir` opcodes based on `is_dir`.
    pub async fn open(
        &mut self,
        nodeid: u64,
        flags: u32,
        is_dir: bool,
    ) -> Result<FuseResponse<fuse_open_out>, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let opcode = if is_dir {
            fuse_opcode::FUSE_OPENDIR.into()
        } else {
            fuse_opcode::FUSE_OPEN.into()
        };

        let in_header = fuse_in_header {
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

        let payload = fuse_open_in {
            flags,
            open_flags: 0,
        };

        let request = FuseRequest {
            in_header: in_header,
            in_payload: Some(payload),
            out_header: fuse_out_header::default(),
            out_payload: Some(fuse_open_out::default()),
        };

        let response = self.transporter.send_request(request).await?;

        match response.out_header.error {
            0 => Ok(response),
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn getattr(
        &mut self,
        nodeid: u64,
        fh: Option<u64>,
    ) -> Result<FuseResponse<fuse_attr_out>, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let in_header = fuse_in_header {
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

        let payload = fuse_getattr_in {
            getattr_flags: if fh.is_some() { FUSE_GETATTR_FH } else { 0 },
            dummy: 0,
            fh: if fh.is_some() { fh.unwrap() } else { 0 },
        };

        let request = FuseRequest {
            in_header: in_header,
            in_payload: Some(payload),
            out_header: fuse_out_header::default(),
            out_payload: Some(fuse_attr_out::default()),
        };

        let response = self.transporter.send_request(request).await?;

        match response.out_header.error {
            0 => Ok(response),
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn readdir(
        &mut self,
        nodeid: u64,
        fh: u64,
        offset: u64,
    ) -> Result<FuseResponse<FuseReaddirOut>, FuseError> {
        let unique = self
            .next_unique
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let in_header = fuse_in_header {
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

        let payload = fuse_read_in {
            fh,
            offset,
            size: FUSE_READDIR_BUFFER_SIZE,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };

        let request = FuseRequest {
            in_header,
            in_payload: Some(payload),
            out_header: fuse_out_header::default(),
            // TODO: optimize
            out_payload: Some(FuseReaddirOut::new()),
        };

        let response = self.transporter.send_request(request).await?;

        match response.out_header.error {
            0 => Ok(response),
            err_code => Err(FuseError::HostError(err_code)),
        }
    }

    pub async fn lookup(
        &mut self,
        root_nodeid: u64,
        name: &str,
    ) -> Result<FuseResponse<fuse_entry_out>, FuseError> {
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

        let in_header = fuse_in_header {
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
            in_header,
            in_payload: Some(name_buf),
            out_header: fuse_out_header::default(),
            out_payload: Some(fuse_entry_out::default()),
        };

        let response = self.transporter.send_request(request).await?;

        match response.out_header.error {
            0 => Ok(response),
            err_code => Err(FuseError::HostError(err_code)),
        }
    }
}
