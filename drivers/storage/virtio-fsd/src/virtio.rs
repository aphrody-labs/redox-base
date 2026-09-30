use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::dma::Dma;
use log::{info, warn};
use virtio_core::spec::{Buffer, ChainBuilder, DescriptorFlags};
use virtio_core::transport::Queue;
use virtio_core::{Device, DmaPool};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::fuse::{
    AllocStrategy, FuseRequest, FuseResponse, FuseTransporter, OutPayload, TransportError,
    fuse_out_header,
};

pub const VIRTIO_FS_TAG_SIZE: usize = 36;
pub const VIRTIO_FS_F_NOTIFICATION: u32 = 0;

const DMA_POOL_CAP: usize = 128;

#[repr(C)]
struct VirtioFsDeviceConfig {
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    num_request_queues: u32,
    notify_buf_size: u32,
}

pub struct VirtioFsSession {
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    hiprio_queue: Arc<Queue>,
    /// Only exists if VIRTIO_FS_F_NOTIFICATION is set.
    notif_queue: Option<Arc<Queue>>,
    /// Only exists if VIRTIO_FS_F_NOTIFICATION is set.
    notif_buf_size: Option<u32>,
    request_queues: Vec<Arc<Queue>>,
    next_queue: AtomicUsize,
    dma_pool: Arc<DmaPool>,
}

impl<'a> VirtioFsSession {
    /// Creates a new VirtioFsSession by initializing the given VirtIO device.
    ///
    /// # Panics
    /// This function panics if the device does not support the required features, or if the device configuration is invalid.
    pub fn new(device: &'a Device) -> Self {
        let notif_supported = device
            .transport
            .check_device_feature(VIRTIO_FS_F_NOTIFICATION);

        if notif_supported {
            info!("virtio-fsd: device supports notifications.");
            device
                .transport
                .ack_driver_feature(VIRTIO_FS_F_NOTIFICATION);
        } else {
            info!("virtio-fsd: device does not support notifications.");
        }

        device.transport.finalize_features();

        let config = device.device_space as *const VirtioFsDeviceConfig;
        assert!(
            config.is_aligned(),
            "virtio-fsd: device configuration is not properly aligned."
        );

        // SAFETY:
        // - `config` is aligned for `VirtioFsDeviceConfig`, so each field is aligned.
        // - `&raw const` does not create a reference to device memory.
        // - The device configuration is valid to read for the duration of these loads.
        // - The tag is read one byte at a time, as the specification requires 8-bit wide
        //   accesses for 8-bit wide fields.
        let tag_ptr = unsafe { &raw const (*config).tag }.cast::<u8>();
        let tag_raw = core::array::from_fn(|i| unsafe { ptr::read_volatile(tag_ptr.add(i)) });
        let nrq =
            u32::from_le(unsafe { ptr::read_volatile(&raw const (*config).num_request_queues) });
        let nbs = notif_supported.then(|| {
            u32::from_le(unsafe { ptr::read_volatile(&raw const (*config).notify_buf_size) })
        });

        // Virtqueues - https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-4600002
        // 0 - hiprio
        // 1 - notification queue which exists if VIRTIO_FS_F_NOTIFICATION is set.
        // 2…n - request queues

        // hiprio queue is only used for FUSE_INTERRUPT, FUSE_FORGET, and FUSE_BATCH_FORGET requests.
        let hiprio_queue = device
            .setup_queue()
            .expect("virtio-fsd: failed to setup hiprio queue.");

        let notif_queue = if notif_supported {
            let nq = device
                .setup_queue()
                .expect("virtio-fsd: failed to setup notification queue.");

            Some(nq)
        } else {
            None
        };

        assert!(
            nrq >= 1,
            "virtio-fsd: at least one request queue is needed."
        );

        // hiprio queue, the notification queue if any, and the request queues.
        let queues_needed = 1 + u64::from(notif_supported) + u64::from(nrq);
        assert!(
            queues_needed <= u64::from(device.transport.num_queues()),
            "virtio-fsd: device reports {nrq} request queues but only supports {} queues.",
            device.transport.num_queues()
        );

        let request_queues: Vec<_> = (0..nrq)
            .map(|_| {
                device
                    .setup_queue()
                    .expect("virtio-fsd: failed to setup request queue.")
            })
            .collect();

        // running this before `run_device`, so an allocation failure happens before the device goes live.
        let dma_pool = DmaPool::new(DMA_POOL_CAP).expect("virtio-fsd: DMA allocation failed.");

        device.transport.run_device();

        Self {
            tag: tag_raw,
            notif_buf_size: nbs,
            hiprio_queue,
            notif_queue,
            request_queues,
            next_queue: AtomicUsize::new(0),
            dma_pool,
        }
    }

    /// Returns the VirtIO FS tag associated with this session.
    ///
    /// # Panics
    /// This function panics if the tag is empty or is not valid UTF-8.
    pub fn tag(&self) -> &str {
        // The tag is NUL-padded, but it is not NUL-terminated if it fills all
        // `VIRTIO_FS_TAG_SIZE` bytes.
        let tag_len = self
            .tag
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(self.tag.len());
        let tag = core::str::from_utf8(&self.tag[..tag_len])
            .expect("virtio-fsd: tag is not valid UTF-8.");

        assert!(!tag.is_empty(), "virtio-fsd: tag is empty.");

        tag
    }

    async fn send(&self, chain: Vec<Buffer>) -> Result<u32, TransportError> {
        let n = self.request_queues.len();
        let i = self.next_queue.fetch_add(1, Ordering::Relaxed) % n;
        Ok(self.request_queues[i].send(chain).await)
    }

    async fn send_hiprio(&self, chain: Vec<Buffer>) -> Result<(), TransportError> {
        self.hiprio_queue.send(chain).await;
        Ok(())
    }
}

impl FuseTransporter for VirtioFsSession {
    /// Sends a FUSE request that expects at least a response header into the default queue.
    async fn send<I, O>(
        &self,
        request: FuseRequest<I>,
        strategy: AllocStrategy<'_>,
    ) -> Result<FuseResponse<O>, TransportError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable,
    {
        let mut builder = ChainBuilder::new();

        // write fuse_in_header and fuse_*_in into a DMA memory

        let mut tx_guard = self.dma_pool.lease().ok_or(TransportError::OutOfMemory)?;
        let mut tx_offset = 0;

        builder = builder.chain(tx_guard.append(&mut tx_offset, request.fuse_in_header.as_bytes()));
        if let Some(ref fuse_in) = request.fuse_in {
            builder = builder.chain(tx_guard.append(&mut tx_offset, fuse_in.as_bytes()));
        }

        // write fuse_out_header into a DMA memory

        let mut rx_guard = self.dma_pool.lease().ok_or(TransportError::OutOfMemory)?;
        let mut rx_offset = 0;

        builder = builder.chain(rx_guard.reserve(&mut rx_offset, size_of::<fuse_out_header>()));

        // decide how to send the data based on AllocStrategy

        // read back once virtio finishes
        let mut rx_bounce = None;

        let mut out_payload_offset = 0;

        match strategy {
            AllocStrategy::DmaPool => {
                out_payload_offset = rx_offset;
                builder = builder.chain(rx_guard.reserve(&mut rx_offset, size_of::<O>()));
            }
            AllocStrategy::DmaPoolHeaderOnly => {}
            AllocStrategy::Send { data } => {
                let size = data.len();

                let dma_uninit =
                    Dma::zeroed_slice(size).map_err(|_| TransportError::OutOfMemory)?;
                // SAFETY:
                // - We only read from this region after host has indicated a successful write
                let mut dma = unsafe { dma_uninit.assume_init() };

                dma.copy_from_slice(&data[..size]);
                builder = builder.chain(Buffer::new_unsized(&Arc::new(dma)));

                // 'AllocStrategy::Send' is only used for writes
                // which return a small `fuse_write_out` struct.
                out_payload_offset = rx_offset;
                builder = builder.chain(rx_guard.reserve(&mut rx_offset, size_of::<O>()));
            }
            AllocStrategy::Recv { ref buf } => {
                let dma_uninit =
                    Dma::zeroed_slice(buf.len()).map_err(|_| TransportError::OutOfMemory)?;
                // SAFETY:
                // - We only read from this region after host has indicated a successful write
                let dma = Arc::new(unsafe { dma_uninit.assume_init() });

                builder =
                    builder.chain(Buffer::new_unsized(&dma).flags(DescriptorFlags::WRITE_ONLY));
                rx_bounce = Some(dma);
            }
        }

        // send and wait

        let chain = builder.build();
        let virtio_written = self.send(chain).await?;

        // parse the header

        // header is always at the begining of the DMA region
        let out_header_bytes = &rx_guard[..size_of::<fuse_out_header>()];
        let (parsed_out_header, _) = fuse_out_header::read_from_prefix(out_header_bytes)
            .map_err(|_| TransportError::Generic)?;

        let fuse_written_claim = parsed_out_header.len;
        if virtio_written != fuse_written_claim {
            warn!(
                "virtio-fsd: Potential data corruption. FUSE claimed {} was written, but wrote {}",
                fuse_written_claim, virtio_written
            );
            return Err(TransportError::Generic);
        }

        // parse the data

        let payload_size =
            virtio_written.saturating_sub(size_of::<fuse_out_header>() as u32) as usize;

        let parsed_out_payload = match strategy {
            AllocStrategy::DmaPoolHeaderOnly => OutPayload::None,
            AllocStrategy::DmaPool | AllocStrategy::Send { .. } => {
                // payload starts right after the begining header
                let bytes = &rx_guard[out_payload_offset..out_payload_offset + size_of::<O>()];
                let (payload, _) =
                    O::read_from_prefix(bytes).map_err(|_| TransportError::Generic)?;
                OutPayload::Struct(payload)
            }
            AllocStrategy::Recv { buf } => {
                let dma = rx_bounce.as_ref().unwrap();
                buf[..payload_size].copy_from_slice(&dma[..payload_size]);
                OutPayload::Size(payload_size as usize)
            }
        };

        Ok(FuseResponse {
            fuse_out_header: parsed_out_header,
            payload: parsed_out_payload,
        })
    }

    /// Sends a FUSE request that does not expect a response, into the high-priority queue.
    ///
    /// Opcodes with this behavior:
    /// - `FUSE_FORGET`
    /// - `FUSE_BATCH_FORGET`
    /// - `FUSE_INTERRUPT`
    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), TransportError>
    where
        I: IntoBytes + Immutable,
    {
        let mut builder = ChainBuilder::new();

        let mut tx_guard = self.dma_pool.lease().ok_or(TransportError::OutOfMemory)?;
        let mut tx_offset = 0;

        builder = builder.chain(tx_guard.append(&mut tx_offset, request.fuse_in_header.as_bytes()));
        if let Some(ref fuse_in) = request.fuse_in {
            builder = builder.chain(tx_guard.append(&mut tx_offset, fuse_in.as_bytes()));
        }

        let chain = builder.build();

        self.send_hiprio(chain).await?;

        Ok(())
    }
}
