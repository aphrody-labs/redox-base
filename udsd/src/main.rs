use event::{EventFlags, EventQueue};
use redox_scheme::scheme::register_sync_scheme;
use redox_scheme::Socket;
use scheme_utils::ReadinessBased;

mod common;
mod dgram;
mod scheme;
mod stream;

use self::dgram::UdsDgramScheme;
use self::stream::UdsStreamScheme;

fn main() {
    daemon::Daemon::new(daemon_runner);
}

fn daemon_runner(daemon: daemon::Daemon) -> ! {
    // TODO: Better error handling
    match inner(daemon) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            println!("ipcd failed: {error}");
            std::process::exit(1);
        }
    }
}

fn inner(daemon: daemon::Daemon) -> anyhow::Result<()> {
    event::user_data! {
        enum EventSource {
            UdsStreamSocket,
            UdsDgramSocket,
        }
    }

    // Prepare uds stream scheme
    let uds_stream_socket = Socket::nonblock()
        .map_err(|e| anyhow::anyhow!("failed to create uds stream scheme: {e}"))?;
    let mut uds_stream = UdsStreamScheme::new(&uds_stream_socket, "uds_stream".to_owned())
        .map_err(|e| anyhow::anyhow!("failed to create uds stream scheme: {e}"))?;
    let mut uds_stream_handler = ReadinessBased::new(&uds_stream_socket, 16);

    // Prepare uds dgram scheme
    let uds_dgram_socket = Socket::nonblock()
        .map_err(|e| anyhow::anyhow!("failed to create uds dgram scheme: {e}"))?;
    let mut uds_dgram = UdsDgramScheme::new(&uds_dgram_socket, "uds_dgram".to_owned())
        .map_err(|e| anyhow::anyhow!("failed to create uds dgram scheme: {e}"))?;
    let mut uds_dgram_handler = ReadinessBased::new(&uds_dgram_socket, 16);

    register_sync_scheme(&uds_stream_socket, "uds_stream", &mut uds_stream)
        .map_err(|e| anyhow::anyhow!("failed to register uds stream scheme: {e}"))?;
    register_sync_scheme(&uds_dgram_socket, "uds_dgram", &mut uds_dgram)
        .map_err(|e| anyhow::anyhow!("failed to register uds dgram scheme: {e}"))?;

    daemon.ready();

    // Create event listener for both files
    let event_queue = EventQueue::<EventSource>::new()
        .map_err(|e| anyhow::anyhow!("failed to create event queue: {e}"))?;
    event_queue
        .subscribe(
            uds_stream_socket.inner().raw(),
            EventSource::UdsStreamSocket,
            EventFlags::READ,
        )
        .map_err(|e| anyhow::anyhow!("failed to subscribe uds stream socket: {e}"))?;
    event_queue
        .subscribe(
            uds_dgram_socket.inner().raw(),
            EventSource::UdsDgramSocket,
            EventFlags::READ,
        )
        .map_err(|e| anyhow::anyhow!("failed to subscribe uds dgram socket: {e}"))?;

    libredox::call::setrens(0, 0)?;

    loop {
        let event = event_queue
            .next_event()
            .map_err(|e| anyhow::anyhow!("error occured in event queue: {e}"))?;

        match event.user_data {
            EventSource::UdsStreamSocket => {
                // Unix Domain Socket Stream scheme
                uds_stream_handler.read_and_process_requests(&mut uds_stream)?;
                uds_stream_handler
                    .poll_all_requests(&mut uds_stream)
                    .map_err(|e| anyhow::anyhow!("error occured in poll_all_requests: {e}"))?;
                uds_stream_handler.write_responses()?;
            }
            EventSource::UdsDgramSocket => {
                // Unix Domain Socket Dgram scheme
                uds_dgram_handler.read_and_process_requests(&mut uds_dgram)?;
                uds_dgram_handler
                    .poll_all_requests(&mut uds_dgram)
                    .map_err(|e| anyhow::anyhow!("error occured in poll_all_requests: {e}"))?;
                uds_dgram_handler.write_responses()?;
            }
        }
    }
}
