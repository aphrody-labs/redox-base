use std::sync::Arc;

use event::EventQueue;
use log::{info, warn};
use pcid_interface::PciFunctionHandle;
use redox_path::RedoxScheme;
use redox_scheme::Socket;
use redox_scheme::scheme::register_async_scheme;

use crate::fuse::{FuseSession, FuseTransporter};
use crate::scheme::VirtioFsScheme;
use crate::vfs::Vfs;
use crate::virtio::VirtioFsSession;

mod fuse;
mod linux_abi;
mod scheme;
mod vfs;
mod virtio;

fn main() {
    warn!("virtio-fsd: starting up...");
    pcid_interface::pci_daemon(init);
}

fn init(daemon: daemon::Daemon, mut pcid_handle: PciFunctionHandle) -> ! {
    let pci_config = pcid_handle.config();

    // The share tag is not known yet, so we use the PCI function name for the log file.
    common::setup_logging(
        "storage",
        "pci",
        &format!("virtio-fsd.{}", pci_config.func.name()),
        common::output_level(),
        common::file_level(),
    );

    // PCI

    // Double check that we have the right device.
    // virtio-fs: 0x105A
    assert_eq!(
        pci_config.func.full_device_id.device_id, 0x105A,
        "virtio-fsd: wrong device ID for virtio-fs"
    );

    let device =
        virtio_core::probe_device(&mut pcid_handle).expect("virtio-fsd: failed to probe device.");

    // VirtIO FS

    let virtio_fs_session = VirtioFsSession::new(&device);

    // Each share (PCI function) gets its own scheme, named after its tag.
    let tag = virtio_fs_session.tag();
    assert!(
        RedoxScheme::new(tag).is_some(),
        "virtio-fsd: tag {tag:?} is not usable as a scheme name (ASCII only, no NUL, '/' or ':')."
    );
    let scheme_name = format!("virtio-fsd.{tag}");

    // FUSE

    let fuse_session = match futures::executor::block_on(FuseSession::new(virtio_fs_session)) {
        Ok(s) => s,
        Err(err) => {
            panic!("virtio-fsd: FUSE_INIT failed: {err}");
        }
    };

    scheme_start(daemon, fuse_session, &scheme_name);
}

fn scheme_start<T: FuseTransporter + 'static>(
    daemon: daemon::Daemon,
    fuse_session: FuseSession<T>,
    scheme_name: &str,
) -> ! {
    // Socket must be non-blocking since `scheme.tick()` drains
    // until EAGAIN, then it runs the batch.
    let scheme_file =
        Arc::new(Socket::nonblock().expect("virtio-fsd: failed to create the scheme socket."));

    let event_queue = EventQueue::new().expect("virtio-fsd: failed to create the event queue.");

    event::user_data! {
        enum Source { Scheme }
    }

    event_queue
        .subscribe(
            scheme_file.inner().raw(),
            Source::Scheme,
            event::EventFlags::READ,
        )
        .expect("virtio-fsd: failed to subscribe to the event queue.");

    let vfs = Vfs::new(fuse_session);
    let mut scheme = VirtioFsScheme::new(Arc::clone(&scheme_file), vfs, scheme_name);

    register_async_scheme(scheme_file.as_ref(), scheme_name, &mut scheme)
        .unwrap_or_else(|err| panic!("virtio-fsd: can't register scheme {scheme_name}: {err}"));

    info!("virtio-fsd: Share is available at '/scheme/{scheme_name}'.");

    daemon.ready();

    libredox::call::setrens(0, 0).expect("virtio-fsd: failed to enter the null namespace.");

    for event in event_queue {
        match event
            .expect("virtio-fsd: failed to get next event.")
            .user_data
        {
            Source::Scheme => {
                if let Err(err) = scheme.tick() {
                    warn!("virtio-fsd: error processing scheme requests: {err}");
                }
            }
        }
    }

    futures::executor::block_on(scheme.shutdown());
    std::process::exit(0);
}
