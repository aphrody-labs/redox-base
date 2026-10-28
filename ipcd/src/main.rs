use event::{EventFlags, EventQueue};
use redox_scheme::scheme::register_sync_scheme;
use redox_scheme::Socket;
use scheme_utils::ReadinessBased;

mod chan;
mod shm;

use self::chan::ChanScheme;
use self::shm::ShmScheme;

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
            ChanSocket,
            ShmSocket,
        }
    }

    // Prepare chan scheme
    let chan_socket =
        Socket::nonblock().map_err(|e| anyhow::anyhow!("failed to create chan scheme: {e}"))?;
    let mut chan = ChanScheme::new(&chan_socket);
    let mut chan_handler = ReadinessBased::new(&chan_socket, 16);

    // Prepare shm scheme
    let shm_socket =
        Socket::nonblock().map_err(|e| anyhow::anyhow!("failed to create shm socket: {e}"))?;
    let mut shm = ShmScheme::new();
    let mut shm_handler = ReadinessBased::new(&shm_socket, 16);

    register_sync_scheme(&chan_socket, "chan", &mut chan)
        .map_err(|e| anyhow::anyhow!("failed to register chan scheme: {e}"))?;
    register_sync_scheme(&shm_socket, "shm", &mut shm)
        .map_err(|e| anyhow::anyhow!("failed to register shm scheme: {e}"))?;

    daemon.ready();

    // Create event listener for both files
    let event_queue = EventQueue::<EventSource>::new()
        .map_err(|e| anyhow::anyhow!("failed to create event queue: {e}"))?;
    event_queue
        .subscribe(
            chan_socket.inner().raw(),
            EventSource::ChanSocket,
            EventFlags::READ,
        )
        .map_err(|e| anyhow::anyhow!("failed to subscribe chan socket: {e}"))?;
    event_queue
        .subscribe(
            shm_socket.inner().raw(),
            EventSource::ShmSocket,
            EventFlags::READ,
        )
        .map_err(|e| anyhow::anyhow!("failed to subscribe shm socket: {e}"))?;

    libredox::call::setrens(0, 0)?;

    loop {
        let event = event_queue
            .next_event()
            .map_err(|e| anyhow::anyhow!("error occured in event queue: {e}"))?;

        match event.user_data {
            EventSource::ChanSocket => {
                // Channel scheme
                chan_handler.read_and_process_requests(&mut chan)?;
                chan_handler
                    .poll_all_requests(&mut chan)
                    .map_err(|e| anyhow::anyhow!("error occured in poll_all_requests: {e}"))?;
                chan_handler.write_responses()?;
            }
            EventSource::ShmSocket => {
                // Shared memory scheme
                shm_handler.read_and_process_requests(&mut shm)?;
                // shm is not a blocking scheme
                shm_handler.write_responses()?;
            }
        }
    }
}
