use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::dma::Dma;
use log::info;
use virtio_core::spec::{Buffer, ChainBuilder, DescriptorFlags};
use virtio_core::transport::{Queue, Retired};
use virtio_core::{Device, DmaPool, PooledPage};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::fuse::{Data, FuseError, FuseReply, FuseRequest, FuseTransporter, fuse_out_header};

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

    async fn send(&self, chain: Vec<Buffer>) -> Result<Retired, FuseError> {
        let n = self.request_queues.len();
        let i = self.next_queue.fetch_add(1, Ordering::Relaxed) % n;
        Ok(self.request_queues[i].send(chain).await)
    }

    /// Appends `request` to a new `chain`. The header and body occupy one page.
    /// `out` follows in the leftover space when it fits, and in its own DMA
    ///  buffer if it doesn't fit in the page.
    fn chained_request<I: IntoBytes + Immutable>(
        &self,
        request: &FuseRequest<I>,
        out: &[u8],
    ) -> Result<ChainBuilder, FuseError> {
        let chain = ChainBuilder::new();

        let mut page = self.dma_pool.lease().ok_or(FuseError::OutOfMemory)?;

        let header = request.fuse_in_header.as_bytes();
        let body = request.fuse_in.as_bytes();

        let len = header.len() + body.len();
        if len > page.len() {
            return Err(FuseError::MalformedPayload);
        }

        page[..header.len()].copy_from_slice(header);
        page[header.len()..len].copy_from_slice(body);

        if out.len() <= page.len() - len {
            page[len..len + out.len()].copy_from_slice(out);
            return Ok(chain.chain(Buffer::new_sized(page, len + out.len())));
        }

        let mut dma = get_dma_init(out.len())?;
        dma.copy_from_slice(out);
        Ok(chain
            .chain(Buffer::new_sized(page, len))
            .chain(Buffer::new(dma)))
    }
}

impl FuseTransporter for VirtioFsSession {
    /// Sends a FUSE request that expects at least a response header into the default queue.
    async fn send<I, O>(
        &self,
        request: FuseRequest<I>,
        data: Data<'_>,
    ) -> Result<FuseReply<O>, FuseError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable,
    {
        // [request page, data out?, reply page, data in?]
        let out = match &data {
            Data::Out(bytes) => *bytes,
            _ => &[],
        };
        let mut chain = self.chained_request(&request, out)?;
        let rx_page = self.dma_pool.lease().ok_or(FuseError::OutOfMemory)?;
        let rx_len = size_of::<fuse_out_header>() + size_of::<O>();
        if rx_len > rx_page.len() {
            return Err(FuseError::MalformedPayload);
        }

        chain = chain.chain(Buffer::new_sized(rx_page, rx_len).flags(DescriptorFlags::WRITE_ONLY));
        let recv_len = match &data {
            Data::In(buf) => buf.len(),
            _ => 0,
        };
        if recv_len > 0 {
            chain = chain
                .chain(Buffer::new(get_dma_init(recv_len)?).flags(DescriptorFlags::WRITE_ONLY));
        }

        let unique = request.fuse_in_header.unique;
        let Retired {
            mut buffers,
            written,
        } = self.send(chain.build()).await?;

        // chain is [tx, data out?, rx, data in?], so the reply side comes off the end
        let bounce = (recv_len > 0).then(|| buffers.pop().unwrap().into_inner::<Dma<[u8]>>());
        let rx_page = buffers.pop().unwrap().into_inner::<PooledPage>();
        drop(buffers);

        FuseReply::from_wire(&rx_page, written, unique, data, bounce.as_deref())
    }

    /// Sends a FUSE request that does not expect a response, into the high-priority queue.
    ///
    /// Opcodes with this behavior:
    /// - `FUSE_FORGET`
    /// - `FUSE_BATCH_FORGET`
    /// - `FUSE_INTERRUPT`
    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), FuseError>
    where
        I: IntoBytes + Immutable,
    {
        let chain = self.chained_request(&request, &[])?.build();

        // dropping the completed chain returns the page to the pool
        self.hiprio_queue.send(chain).await;

        Ok(())
    }
}

fn get_dma_init(len: usize) -> Result<Dma<[u8]>, FuseError> {
    let dma = Dma::<[u8]>::zeroed_slice(len).map_err(|_| FuseError::OutOfMemory)?;
    // SAFETY: a slice of zeros is a valid slice of u8.
    Ok(unsafe { dma.assume_init() })
}
