//! Display Config Server (DCS)
//!
//! A Wayland compositor that owns and leases out displays for use by Vulkan Direct-to-Display
//! (D2D) applications. The user launches this server to initialize and configure a set of
//! displays. VK_KHR_display apps can then inherit this configuration on startup instead of
//! re-initializing. This is used for display walls which have strong synchronization requirements
//! and may take substantial time to initialize.
//!
//! In environments that do not have an X server (vulkan direct to display, wayland) there is
//! currently no way to persistently configure displays. This poses a problem for advanced display
//! features, where configuring a display wall or setting a particular mode is an expensive
//! operation and it may not be desirable to repeat it. Persistence is also an issue, as there is
//! no common server that "owns" the display and can hold it in whatever the configured mode is.
//! Additionally there is no mechanism for specifying a configuration for a display, or existing
//! projects which would do such a thing.
//!
//! This project is the "display config server", which will acquire a display, configure it
//! according to what the user requested, and lease it out to consumers as requested. This allows
//! for persistent configurations, and provides infrastructure for advanced display features which
//! can be consumed by the leasing client.
//!
//! # System overview
//!
//! DCS sits at the centre of a three-party system:
//!
//! - **Display Config Server** (this binary) — acquires one or more DRM
//!   displays at startup, renders a splash screen while idle, and re-leases
//!   each display to Vulkan D2D clients on demand via the
//!   `wp_drm_lease_device_v1` protocol. A private Wayland protocol
//!   (`zwp_display_config_server_v1`) allows the configuration tool to query
//!   and modify display settings atomically.
//!
//! - **Dynamic Configuration Tool** — CLI tool that reads a
//!   saved configuration from disk at startup and forwards it to the server
//!   over the private protocol. Users also invoke it directly to change
//!   settings at runtime.
//!
//! - **VK_KHR_display Vulkan app** — connects to the DCS Wayland socket,
//!   discovers the pre-configured display via `wp_drm_lease_device_v1`, and
//!   drives it directly without issuing a modeset (the display link is already
//!   trained by DCS, preserving framelock across lease transitions).
//!
//! # IPC
//!
//! Wayland is used as the IPC mechanism: it naturally models double-buffered configuration
//! commits, and the upstream DRM leasing protocol (`wp_drm_lease_device_v1`) is already
//! community-supported. This server is built on [Smithay], a Rust Wayland compositor toolkit.
//!
//! [Smithay]: https://github.com/Smithay/smithay

mod drm_output;
mod protocols;
mod zwp_display_config_server_v1;

use std::sync::Arc;

use drm_output::DcsOutput;
use protocols::zwp_display_config_server_v1::zwp_dcs_manager::ZwpDcsManager;
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

    // Advertise the DCS private protocol in the registry so clients can bind
    // the manager object.
    display
        .handle()
        .create_global::<DcsState, ZwpDcsManager, _>(1, ());

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
