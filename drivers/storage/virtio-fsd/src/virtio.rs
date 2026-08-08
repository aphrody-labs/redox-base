use common::dma::Dma;
use core::slice;
use log::warn;
use std::{
    ops::{Deref, DerefMut},
    ptr,
    sync::Arc,
};
use syscall::PAGE_SIZE;
use virtio_core::{
    Device,
    spec::{Buffer, ChainBuilder, DescriptorFlags},
    transport::Queue,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::fuse::{
    AllocStrategy, FuseRequest, FuseResponse, FuseTransporter, OutPayload, TransportError,
    fuse_out_header,
};

pub const VIRTIO_FS_TAG_SIZE: usize = 36;
pub const VIRTIO_FS_F_NOTIFICATION: u32 = 0;

const DMA_POOL_CAP: usize = 128;

#[repr(C, packed)]
struct VirtioFsDeviceConfig {
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    num_request_queues: u32,
    notify_buf_size: u32,
}

struct DmaPool {
    pool: Dma<[u8]>,
    free_indexes: crossbeam_queue::ArrayQueue<usize>,
}

struct DmaGuardMut<'dma> {
    index: usize,
    phys: usize,
    virt: &'dma mut [u8],
    dma_pool: &'dma DmaPool,
}
impl<'dma> DmaGuardMut<'dma> {
    /// Copies a struct into the DMA memory, advances the offset, and returns a Virtio Buffer.
    pub fn append_tx<T: IntoBytes + Immutable>(&mut self, offset: &mut usize, data: &T) -> Buffer {
        let bytes = data.as_bytes();
        let len = bytes.len();
        self.virt[*offset..(*offset + len)].copy_from_slice(bytes);

        // SAFETY
        // - `self.phys` was handed to us by `Dma::zeroed_slice`.
        let buf = unsafe { Buffer::from_raw_parts(self.phys + *offset, len) };

        *offset += len;

        buf
    }

    /// Reserves space for the device to write into, advances the offset, and returns a Write-Only Buffer.
    pub fn reserve_rx<T: Sized>(&mut self, offset: &mut usize) -> Buffer {
        let len = core::mem::size_of::<T>();

        // SAFETY
        // - `self.phys` was handed to us by `Dma::zeroed_slice`.
        let buf = unsafe { Buffer::from_raw_parts(self.phys + *offset, len) }
            .flags(DescriptorFlags::WRITE_ONLY);

        *offset += len;

        buf
    }
}
impl<'dma> Deref for DmaGuardMut<'dma> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.virt
    }
}
impl<'dma> DerefMut for DmaGuardMut<'dma> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.virt
    }
}
impl<'dma> Drop for DmaGuardMut<'dma> {
    fn drop(&mut self) {
        self.dma_pool.put_memory(self.index);
    }
}

impl DmaPool {
    fn new() -> Self {
        let dma = Dma::zeroed_slice(DMA_POOL_CAP * PAGE_SIZE)
            .expect("virtio-fsd: DMA allocation failed.");

        // SAFETY:
        // - Any value can be considered a valid u8.
        let pool = unsafe { dma.assume_init() };

        let free_indexes = crossbeam_queue::ArrayQueue::new(DMA_POOL_CAP);

        for i in 0..DMA_POOL_CAP {
            free_indexes
                .push(i)
                .expect("virtio-fsd: free_indexes is full.");
        }

        Self { pool, free_indexes }
    }
    fn get_memory_mut<'dma>(&'dma self) -> Option<DmaGuardMut<'dma>> {
        let i = self.free_indexes.pop()?;
        let offset = i * PAGE_SIZE;

        let virt = {
            let base_ptr = self.pool.as_ptr() as *mut u8;
            // SAFETY
            // - It is guaranteed that the offset is always within the bounds of the original allocation,
            //   because we control pushes and pops from the queue.
            // - `free_indexes` is a MPMC queue, hence when given an index, we are the only owner of it.
            // - (As a result) the given memory range will only have one owner.
            unsafe {
                let slot_ptr = base_ptr.add(offset);
                slice::from_raw_parts_mut(slot_ptr, PAGE_SIZE)
            }
        };
        Some(DmaGuardMut {
            index: i,
            phys: self.pool.physical() + offset,
            virt,
            dma_pool: self,
        })
    }
    fn put_memory(&self, index: usize) {
        self.free_indexes
            .push(index)
            .expect("virtio-fsd: free_indexes queue was full which is not expected.")
    }
}

pub struct VirtioFsSession<'a> {
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    hiprio_queue: Arc<Queue<'a>>,
    /// Only exists if VIRTIO_FS_F_NOTIFICATION is set.
    notif_queue: Option<Arc<Queue<'a>>>,
    /// Only exists if VIRTIO_FS_F_NOTIFICATION is set.
    notif_buf_size: Option<u32>,
    request_queues: Vec<Arc<Queue<'a>>>,
    dma_pool: DmaPool,
}

impl<'a> VirtioFsSession<'a> {
    /// Creates a new VirtioFsSession by initializing the given VirtIO device.
    ///
    /// # Panics
    /// This function panics if the device does not support the required features, or if the device configuration is invalid.
    pub fn new(device: &'a Device) -> Self {
        let notif_supported = device
            .transport
            .check_device_feature(VIRTIO_FS_F_NOTIFICATION);

        if notif_supported {
            warn!("virtio-fsd: device supports notifications.");
            device
                .transport
                .ack_driver_feature(VIRTIO_FS_F_NOTIFICATION);
        } else {
            warn!("virtio-fsd: device does not support notifications.");
        }

        device.transport.finalize_features();

        let config = device.device_space as *const VirtioFsDeviceConfig;

        // SAFETY:
        // - We use addr_of! to avoid creating a reference to a packed field.
        // - Tag has alignment of 1, so it's always safe to read it as a byte array.
        let tag_raw = unsafe { ptr::read_volatile(ptr::addr_of!((*config).tag)) };

        // SAFETY:
        // - We use addr_of! to avoid creating a reference to a packed field, that can potentially be unaligned.
        let nrq_ptr = unsafe { ptr::addr_of!((*config).num_request_queues) };
        assert_eq!(
            nrq_ptr as usize % core::mem::align_of::<u32>(),
            0,
            "virtio-fsd: num_request_queues is not properly aligned."
        );

        // SAFETY:
        // - nrq_ptr has alignment of 4, so it's safe to read it.
        let nrq = u32::from_le(unsafe { ptr::read_volatile(nrq_ptr) });

        let nbs = if notif_supported {
            // SAFETY:
            // - We use addr_of! to avoid creating a reference to a packed field, that can potentially be unaligned.
            let nbs_ptr = unsafe { ptr::addr_of!((*config).notify_buf_size) };
            assert_eq!(
                nbs_ptr as usize % core::mem::align_of::<u32>(),
                0,
                "virtio-fsd: notify_buf_size is not properly aligned."
            );

            // SAFETY:
            // - nbs_ptr has alignment of 4, so it's safe to read it.
            Some(u32::from_le(unsafe { ptr::read_volatile(nbs_ptr) }))
        } else {
            None
        };

        // Virtqueues - https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-4600002
        // 0 - hiprio
        // 1 - notification queue which exists if VIRTIO_FS_F_NOTIFICATION is set.
        // 2…n - request queues

        // hiprio queue is only used for FUSE_INTERRUPT, FUSE_FORGET, and FUSE_BATCH_FORGET requests.
        let hiprio_queue = device
            .transport
            .setup_queue(virtio_core::MSIX_PRIMARY_VECTOR, &device.irq_handle)
            .expect("virtio-fsd: failed to setup hiprio queue.");

        let notif_queue = if notif_supported {
            let nq = device
                .transport
                .setup_queue(virtio_core::MSIX_PRIMARY_VECTOR, &device.irq_handle)
                .expect("virtio-fsd: failed to setup notification queue.");

            Some(nq)
        } else {
            None
        };

        assert!(
            nrq >= 1,
            "virtio-fsd: at least one request queue is needed."
        );
        let request_queues = (0..nrq)
            .map(|_| {
                device
                    .transport
                    .setup_queue(virtio_core::MSIX_PRIMARY_VECTOR, &device.irq_handle)
                    .expect("virtio-fsd: failed to setup request queue.")
            })
            .collect();

        device.transport.run_device();

        Self {
            tag: tag_raw,
            notif_buf_size: nbs,
            hiprio_queue,
            notif_queue,
            request_queues,
            dma_pool: DmaPool::new(),
        }
    }

    /// Returns the VirtIO FS tag associated with this session.
    ///
    /// # Panics
    /// This function panics if the tag is not valid UTF-8.
    pub fn tag(&self) -> &str {
        let tag_len = self
            .tag
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(self.tag.len());

        core::str::from_utf8(&self.tag[..tag_len]).expect("virtio-fsd: tag is not valid UTF-8.")
    }

    async fn send(&self, chain: Vec<Buffer>) -> u32 {
        //TODO: round-robin or per-cpu?
        self.request_queues[0].send(chain).await
    }

    async fn send_hiprio(&self, chain: Vec<Buffer>) {
        self.hiprio_queue.send(chain).await;
    }
}

impl FuseTransporter for VirtioFsSession<'_> {
    /// Sends a FUSE request that expects at least a response header into the default queue.
    ///
    /// # Panics
    /// This function panics if the underlying send operation fails. This happens because `virtio-core`'s `send()` method uses `unwrap()`.
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

        let mut tx_guard = self
            .dma_pool
            .get_memory_mut()
            .ok_or(TransportError::OutOfMemory)?;
        let mut tx_offset = 0;

        builder = builder.chain(tx_guard.append_tx(&mut tx_offset, &request.fuse_in_header));
        if let Some(ref fuse_in) = request.fuse_in {
            builder = builder.chain(tx_guard.append_tx(&mut tx_offset, fuse_in));
        }

        // write fuse_out_header into a DMA memory

        let mut rx_guard = self
            .dma_pool
            .get_memory_mut()
            .ok_or(TransportError::OutOfMemory)?;
        let mut rx_offset = 0;

        builder = builder.chain(rx_guard.reserve_rx::<fuse_out_header>(&mut rx_offset));

        // decide how to send the data based on AllocStrategy

        // keep the DMA memory alive until virtio finishes
        let mut _tx_bounce = None;
        let mut rx_bounce = None;

        let mut out_payload_offset = 0;

        match strategy {
            AllocStrategy::DmaPool => {
                out_payload_offset = rx_offset;
                builder = builder.chain(rx_guard.reserve_rx::<O>(&mut rx_offset));
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
                builder = builder.chain(Buffer::new_unsized(&dma));
                _tx_bounce = Some(dma);

                // 'AllocStrategy::Send' is only used for writes
                // which return a small `fuse_write_out` struct.
                out_payload_offset = rx_offset;
                builder = builder.chain(rx_guard.reserve_rx::<O>(&mut rx_offset));
            }
            AllocStrategy::Recv { ref buf } => {
                let dma_uninit =
                    Dma::zeroed_slice(buf.len()).map_err(|_| TransportError::OutOfMemory)?;
                // SAFETY:
                // - We only read from this region after host has indicated a successful write
                let dma = unsafe { dma_uninit.assume_init() };

                builder =
                    builder.chain(Buffer::new_unsized(&dma).flags(DescriptorFlags::WRITE_ONLY));
                rx_bounce = Some(dma);
            }
        }

        // these must not exceed PAGE_SIZE because dma_pool only gives out PAGE_SIZEd chunks.
        assert!(tx_offset <= PAGE_SIZE, "virtio-fsd: TX Pool Overflow");
        assert!(rx_offset <= PAGE_SIZE, "virtio-fsd: RX Pool Overflow");

        // send and wait

        let chain = builder.build();
        let virtio_written = self.send(chain).await;

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
    ///
    /// # Panics
    /// This function panics if the underlying send operation fails. This happens because `virtio-core`'s `send()` method uses `unwrap()`.
    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), TransportError>
    where
        I: IntoBytes + Immutable,
    {
        let mut builder = ChainBuilder::new();

        let mut tx_guard = self
            .dma_pool
            .get_memory_mut()
            .ok_or(TransportError::OutOfMemory)?;
        let mut tx_offset = 0;

        builder = builder.chain(tx_guard.append_tx(&mut tx_offset, &request.fuse_in_header));
        if let Some(ref fuse_in) = request.fuse_in {
            builder = builder.chain(tx_guard.append_tx(&mut tx_offset, fuse_in));
        }

        assert!(tx_offset <= PAGE_SIZE, "virtio-fsd: TX Pool Overflow");

        let chain = builder.build();

        self.send_hiprio(chain).await;

        Ok(())
    }
}
