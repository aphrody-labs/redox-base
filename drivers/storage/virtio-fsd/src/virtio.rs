use common::dma::Dma;
use log::warn;
use std::{ptr, sync::Arc};
use virtio_core::{
    Device,
    spec::{Buffer, ChainBuilder, DescriptorFlags},
    transport::Queue,
};

use crate::fuse::{FuseRequest, FuseResponse, FuseTransporter, TransportError};

pub const VIRTIO_FS_TAG_SIZE: usize = 36;
pub const VIRTIO_FS_F_NOTIFICATION: u32 = 0;

#[repr(C, packed)]
struct VirtioFsDeviceConfig {
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    num_request_queues: u32,
    notify_buf_size: u32,
}

pub struct VirtioFsSession<'a> {
    // TODO: we probably don't need all three fields from VirtioFsDeviceConfig.
    tag: [u8; VIRTIO_FS_TAG_SIZE],
    pub num_request_queues: u32,
    pub notify_buf_size: u32,
    // TODO: we currently only support one request queue, but we should support more if the device has them.
    request_queue: Arc<Queue<'a>>,
    /// The hiprio queue follows the same request format as the request queues. This queue must only be used for `FUSE_INTERRUPT`, `FUSE_FORGET`, and `FUSE_BATCH_FORGET` requests.
    hiprio_queue: Arc<Queue<'a>>,
}

impl<'a> VirtioFsSession<'a> {
    /// Creates a new VirtioFsSession by initializing the given VirtIO device.
    ///
    /// # Panics
    /// This function panics if the device does not support the required features, or if the device configuration is invalid.
    pub fn new(device: &'a Device) -> Self {
        let supports_notif = device
            .transport
            .check_device_feature(VIRTIO_FS_F_NOTIFICATION);

        if supports_notif {
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
        let nbs = u32::from_le(unsafe { ptr::read_volatile(nbs_ptr) });

        // hiprio queue is only used for FUSE_INTERRUPT, FUSE_FORGET, and FUSE_BATCH_FORGET requests.
        let hiprio_queue = device
            .transport
            .setup_queue(virtio_core::MSIX_PRIMARY_VECTOR, &device.irq_handle)
            .expect("virtio-fsd: failed to setup hiprio queue.");

        // TODO: If the device has more than 1 request queues (nrq above), we should use them.
        let request_queue = device
            .transport
            .setup_queue(virtio_core::MSIX_PRIMARY_VECTOR, &device.irq_handle)
            .expect("virtio-fsd: failed to setup request queue.");

        device.transport.run_device();

        Self {
            tag: tag_raw,
            num_request_queues: nrq,
            notify_buf_size: nbs,
            hiprio_queue,
            request_queue,
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
}

impl FuseTransporter for VirtioFsSession<'_> {
    /// Sends a FUSE request to the device and waits for the response.
    /// The returned `u32` is the number of bytes written by the device in the response payload (not including the out_header).
    /// # Errors
    /// This function returns a `TransportError` if the request could not be sent, or if the device returned an error response.
    ///
    /// # Panics
    /// This function panics if the underlying send operation fails. This happens because `virtio-core`'s `send()` method uses `unwrap()`.
    async fn send_request<I, O>(
        &self,
        request: FuseRequest<I, O>,
    ) -> Result<FuseResponse<O>, TransportError>
    where
        I: Sized,
        O: Sized + Clone,
    {
        // TODO: sized DMA?

        let in_header = Dma::new(request.in_header).map_err(|_| TransportError::OutOfMemory)?;
        let out_header = Dma::new(request.out_header).map_err(|_| TransportError::OutOfMemory)?;
        let in_payload = if let Some(in_payload) = request.in_payload {
            Some(Dma::new(in_payload).map_err(|_| TransportError::OutOfMemory)?)
        } else {
            None
        };
        let out_payload = if let Some(out_payload) = request.out_payload {
            Some(Dma::new(out_payload).map_err(|_| TransportError::OutOfMemory)?)
        } else {
            None
        };

        let mut builder = ChainBuilder::new();
        builder = builder.chain(Buffer::new(&in_header));
        if let Some(ref payload) = in_payload {
            builder = builder.chain(Buffer::new(payload));
        }
        builder = builder.chain(Buffer::new(&out_header).flags(DescriptorFlags::WRITE_ONLY));
        if let Some(ref payload) = out_payload {
            builder = builder.chain(Buffer::new(payload).flags(DescriptorFlags::WRITE_ONLY));
        }
        let chain = builder.build();

        // TODO: send() uses unwrap which is fatal. virtio-core must be updated. then we need to catch send()
        // failures and convert them to TransportError::VirtqueueFull or TransportError::DeviceOffline as appropriate.
        let written = self.request_queue.send(chain).await;

        // TODO: This is ineffient. Scheme layer must allocate and pass down 'out_payload' as a DMA buffer. Then the 'clone' bound for 'O' can be removed.
        // Also it's size must be in accordance with the device's capabilities, which can be determined from the FUSE_INIT response.
        let response = FuseResponse {
            written_len: written,
            out_header: *out_header,
            out_payload: if let Some(payload) = out_payload {
                Some((*payload).clone())
            } else {
                None
            },
        };

        Ok(response)
    }
}
