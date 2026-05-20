use std::os::unix::io::OwnedFd;
use std::sync::Arc;

use smithay::{
    backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent},
    reexports::{
        calloop::EventLoop,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            Display,
        },
    },
    utils::DeviceFd,
    wayland::socket::ListeningSocketSource,
};

use tracing::info;

/// Per-client state.
///
/// Clients connecting over the Wayland socket each get one of these. Empty
/// for now; per-client protocol state will be stored here as protocols are
/// added.
struct ClientState;

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Global compositor state.
///
/// Holds resources that live for the lifetime of the server. As protocols and
/// subsystems are added they will store their state here.
struct State {
    drm_device: DrmDevice,
}

/// Top-level data passed through the calloop event loop.
///
/// calloop callbacks receive `&mut CalloopData`, so both the Wayland display
/// and the compositor state must live here.
struct CalloopData {
    state: State,
    display: Display<State>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up tracing output. The RUST_LOG environment variable controls the
    // filter (e.g. RUST_LOG=debug).
    tracing_subscriber::fmt::init();

    // Accept the DRM device path as an optional first argument so the server
    // can be pointed at any card node. Default is the primary render node on
    // most Linux systems.
    let drm_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card0".to_string());

    info!("Opening DRM device: {}", drm_path);

    // Open the DRM device read/write so we can perform mode-setting.
    let drm_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&drm_path)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(drm_file)));

    // DrmDevice::new returns the device itself plus a DrmDeviceNotifier that
    // must be registered with the event loop to receive VBlank / error events.
    // disable_connectors=false leaves the current display state intact.
    let (drm_device, drm_notifier) = DrmDevice::new(drm_fd, false)?;
    info!("DRM device initialized");

    // -------------------------------------------------------------------------
    // Event loop
    // -------------------------------------------------------------------------

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    // Drive the DRM device: VBlank events signal that a page flip completed and
    // the next frame can be queued.
    loop_handle.insert_source(drm_notifier, |event, _metadata, _data| match event {
        DrmEvent::VBlank(crtc) => {
            tracing::debug!("VBlank on crtc {:?}", crtc);
        }
        DrmEvent::Error(err) => {
            tracing::error!("DRM error: {}", err);
        }
    })?;

    // -------------------------------------------------------------------------
    // Wayland display
    // -------------------------------------------------------------------------

    let display: Display<State> = Display::new()?;

    // ListeningSocketSource wraps a Wayland socket and fires a callback for
    // each new client connection. new_auto() picks the next free wayland-N
    // name under XDG_RUNTIME_DIR.
    let socket = ListeningSocketSource::new_auto()?;
    info!(
        "Listening on Wayland socket: {}",
        socket.socket_name().to_string_lossy()
    );

    loop_handle.insert_source(socket, |client_stream, _, data: &mut CalloopData| {
        data.display
            .handle()
            .insert_client(client_stream, Arc::new(ClientState))
            .expect("Failed to insert Wayland client");
    })?;

    // -------------------------------------------------------------------------
    // Run
    // -------------------------------------------------------------------------

    let mut calloop_data = CalloopData {
        state: State { drm_device },
        display,
    };

    info!("Display config server started");

    // The closure passed to run() is called once after each batch of events has
    // been dispatched. This is where we flush pending Wayland protocol messages
    // back to connected clients.
    event_loop.run(None, &mut calloop_data, |data| {
        data.display
            .dispatch_clients(&mut data.state)
            .expect("Error dispatching Wayland clients");
        data.display
            .flush_clients()
            .expect("Error flushing Wayland clients");
    })?;

    Ok(())
}
