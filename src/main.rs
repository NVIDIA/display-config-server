mod drm_output;

use std::sync::Arc;

use drm_output::DcsOutput;
use smithay::{
    backend::drm::DrmEvent,
    reexports::{
        calloop::EventLoop,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            Display,
        },
    },
    wayland::socket::ListeningSocketSource,
};
use tracing::info;

/// Per-client state.
///
/// Clients connecting over the Wayland socket each get one of these. Empty
/// for now; per-client protocol state will be stored here as protocols are
/// added.
struct DcsClientState;

impl ClientData for DcsClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Global compositor state.
///
/// Holds resources that live for the lifetime of the server.
struct DcsState {
    output: DcsOutput,
}

/// Top-level data passed through the calloop event loop.
///
/// calloop callbacks receive `&mut DcsCalloopData`, so both the Wayland
/// display and the compositor state must live here.
struct DcsCalloopData {
    state: DcsState,
    display: Display<DcsState>,
}

/// Resolve the DRM device path from command-line arguments.
///
/// Supported forms:
///   --card <N>          →  /dev/dri/cardN
///   /dev/dri/cardN      →  used as-is (positional argument)
///
/// Defaults to `/dev/dri/card0` when no argument is given.
fn parse_drm_path() -> Result<String, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--card" => {
                let n = args
                    .next()
                    .ok_or("--card requires a card number argument")?;
                // Validate that it's a non-negative integer.
                n.parse::<u32>()
                    .map_err(|_| format!("--card: '{}' is not a valid card number", n))?;
                return Ok(format!("/dev/dri/card{}", n));
            }
            path if !path.starts_with('-') => {
                return Ok(path.to_string());
            }
            unknown => {
                return Err(format!("unknown argument: {}", unknown).into());
            }
        }
    }
    Ok("/dev/dri/card0".to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up tracing output. RUST_LOG controls the filter (e.g. RUST_LOG=debug).
    tracing_subscriber::fmt::init();

    // Parse --card <N> to select /dev/dri/cardN, or accept a full device path
    // as a positional argument.  Defaults to card0.
    let drm_path = parse_drm_path()?;

    info!("Initialising DRM device: {}", drm_path);

    // Build the output – this opens the device, picks a connected connector,
    // creates the GBM/EGL/GLES stack, and returns a notifier for VBlanks.
    let (mut output, drm_notifier) = DcsOutput::new(&drm_path)?;
    // Do the first draw to initialize the screen(s) contents
    output.render().expect("render failed");
    info!("DRM output ready");

    // -------------------------------------------------------------------------
    // Event loop
    // -------------------------------------------------------------------------

    let mut event_loop: EventLoop<DcsCalloopData> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    // VBlank events signal that a page flip completed; tell the compositor so
    // it can retire the old buffer and allow a new frame to be queued.
    loop_handle.insert_source(drm_notifier, |event, _metadata, data| match event {
        DrmEvent::VBlank(_crtc) => {
            data.state
                .output
                .frame_submitted()
                .expect("frame_submitted failed");
        }
        DrmEvent::Error(err) => {
            tracing::error!("DRM error: {}", err);
        }
    })?;

    // -------------------------------------------------------------------------
    // Wayland display
    // -------------------------------------------------------------------------

    let display: Display<DcsState> = Display::new()?;

    // ListeningSocketSource fires a callback for each new client connection.
    // new_auto() picks the next free wayland-N name under XDG_RUNTIME_DIR.
    let socket = ListeningSocketSource::with_name("display-config-server-0")?;
    info!(
        "Listening on Wayland socket: {}",
        socket.socket_name().to_string_lossy()
    );

    loop_handle.insert_source(socket, |client_stream, _, data: &mut DcsCalloopData| {
        data.display
            .handle()
            .insert_client(client_stream, Arc::new(DcsClientState))
            .expect("Failed to insert Wayland client");
    })?;

    // -------------------------------------------------------------------------
    // Run
    // -------------------------------------------------------------------------

    let mut calloop_data = DcsCalloopData {
        state: DcsState { output },
        display,
    };

    info!("Display config server started");

    // The post-dispatch callback runs after each batch of events.  Render
    // here so any state change driven by events is reflected on screen, then
    // flush pending Wayland protocol messages back to clients.
    event_loop.run(None, &mut calloop_data, |data| {
        data.state.output.render().expect("render failed");
        data.display
            .dispatch_clients(&mut data.state)
            .expect("Error dispatching Wayland clients");
        data.display
            .flush_clients()
            .expect("Error flushing Wayland clients");
    })?;

    Ok(())
}
