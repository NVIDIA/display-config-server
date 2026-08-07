// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
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

mod attribute;
mod protocol;
mod protocols;
mod render;

use std::sync::Arc;

use render::DcsDevice;
use protocols::zwp_display_config_server_v1::zwp_dcs_manager::ZwpDcsManager;
use smithay::{
    backend::drm::{DrmEvent, DrmNode},
    delegate_dispatch2,
    reexports::{
        calloop::{
            generic::Generic,
            EventLoop, Interest, Mode, PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            Display,
        },
    },
    wayland::{
        drm_lease::{
            DrmLease, DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState,
            LeaseRejected,
        },
        socket::ListeningSocketSource,
    },
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
    devices: Vec<DcsDevice>,
    /// Pending attribute changes accumulated by sub-protocol handlers before a
    /// topology commit fires.  `None` when no sub-protocol has staged changes.
    pending_commit: Option<attribute::PendingCommit>,
}

impl DcsState {
    /// Find the [`DcsOutput`] whose compositor drives `crtc`, searching across
    /// all devices.  Returns `None` when no output owns that CRTC.
    fn output_for_crtc(&self, crtc: drm::control::crtc::Handle) -> Option<&render::dcs_output::DcsOutput> {
        self.devices.iter().find_map(|d| d.outputs.get(&crtc))
    }

    /// Mutable variant of [`output_for_crtc`].
    fn output_for_crtc_mut(&mut self, crtc: drm::control::crtc::Handle) -> Option<&mut render::dcs_output::DcsOutput> {
        self.devices.iter_mut().find_map(|d| d.outputs.get_mut(&crtc))
    }

    /// Test whether the requested mode is accepted by the hardware without
    /// applying it, delegating to the owning [`DcsDevice`].
    fn validate_output_mode_change(
        &self,
        crtc: drm::control::crtc::Handle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        for device in &self.devices {
            if device.outputs.contains_key(&crtc) {
                return device.validate_output_mode_change(crtc, width, height, refresh_mhz);
            }
        }
        anyhow::bail!("no device found for CRTC {:?}", crtc)
    }

    /// Apply the requested mode to the output driving `crtc`, delegating to
    /// the owning [`DcsDevice`].
    fn commit_output_mode_change(
        &mut self,
        crtc: drm::control::crtc::Handle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        for device in &mut self.devices {
            if device.outputs.contains_key(&crtc) {
                return device.commit_output_mode_change(crtc, width, height, refresh_mhz);
            }
        }
        anyhow::bail!("no device found for CRTC {:?}", crtc)
    }
}

impl DrmLeaseHandler for DcsState {
    fn drm_lease_state(&mut self, node: DrmNode) -> &mut DrmLeaseState {
        self.devices
            .iter_mut()
            .find(|d| d.drm_node == node)
            .and_then(|d| d.drm_lease_state.as_mut())
            .expect("no DrmLeaseState for node")
    }

    fn lease_request(
        &mut self,
        _node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        // Find the device that owns the requested connectors.
        let device = self
            .devices
            .iter()
            .find(|d| {
                request.connectors.iter().all(|c| {
                    d.outputs.values().any(|o| o.connector_handle == *c)
                })
            })
            .ok_or_else(LeaseRejected::default)?;

        let mut builder = DrmLeaseBuilder::new(&device.drm_device);

        for &conn in &request.connectors {
            builder.add_connector(conn);

            // Find the output driving this connector and add its CRTC and
            // primary plane. DRM_IOCTL_MODE_CREATE_LEASE requires at least one
            // of each when DRM_CLIENT_CAP_UNIVERSAL_PLANES is enabled.
            if let Some((&crtc, output)) = device.outputs.iter().find(|(_, o)| o.connector_handle == conn) {
                builder.add_crtc(crtc);
                if let Some((plane, claim)) = output.primary_plane_with_claim() {
                    builder.add_plane(plane, claim);
                }
            }
        }

        Ok(builder)
    }

    fn new_active_lease(&mut self, _node: DrmNode, lease: DrmLease) {
        info!("DRM lease {} granted", lease.id());
        // Move the lease into the matching output. DrmLease::Drop revokes the
        // kernel lease, so we must move (not clone) it into persistent storage.
        let mut lease = Some(lease);
        'outer: for device in &mut self.devices {
            for output in device.outputs.values_mut() {
                if lease.as_ref().map_or(false, |l| l.connectors().any(|c| *c == output.connector_handle)) {
                    output.active_lease = lease.take();
                    break 'outer;
                }
            }
        }
    }

    fn lease_destroyed(&mut self, _node: DrmNode, lease_id: u32) {
        info!("DRM lease {} destroyed", lease_id);
        for device in &mut self.devices {
            for output in device.outputs.values_mut() {
                if output.active_lease.as_ref().map_or(false, |l| l.id() == lease_id) {
                    output.active_lease = None;
                    output.request_redraw();
                }
            }
        }
    }
}

delegate_dispatch2!(DcsState);

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
/// When no argument is given, tries `/dev/dri/card0` first, then falls back
/// to `/dev/dri/card1` if `card0` does not exist.
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

    // No explicit device given: prefer card0, fall back to card1.
    let default = "/dev/dri/card0";
    let fallback = "/dev/dri/card1";
    if std::path::Path::new(default).exists() {
        Ok(default.to_string())
    } else {
        info!("{} not found, falling back to {}", default, fallback);
        Ok(fallback.to_string())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up tracing output. RUST_LOG controls the filter (e.g. RUST_LOG=debug).
    tracing_subscriber::fmt::init();

    // Parse --card <N> to select /dev/dri/cardN, or accept a full device path
    // as a positional argument.  Defaults to card0.
    let drm_path = parse_drm_path()?;

    info!("Initialising DRM device: {}", drm_path);

    // Open the device, create the shared renderer, and enumerate all
    // connected outputs. The initial splash frame is rendered below before
    // the event loop starts.
    let (mut device, drm_notifier) = DcsDevice::new(&drm_path)?;
    device.render().expect("initial render failed");

    // -------------------------------------------------------------------------
    // Event loop
    // -------------------------------------------------------------------------

    let mut event_loop: EventLoop<DcsCalloopData> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    // VBlank events carry the CRTC handle so we can dispatch directly to the
    // output that flipped, leaving other outputs untouched.
    let device_idx = 0usize;
    loop_handle.insert_source(drm_notifier, move |event, _metadata, data| match event {
        DrmEvent::VBlank(crtc) => {
            data.state.devices[device_idx]
                .frame_submitted(crtc)
                .expect("frame_submitted failed");
        }
        DrmEvent::Error(err) => {
            tracing::error!("DRM error: {}", err);
        }
    })?;

    // -------------------------------------------------------------------------
    // Wayland display
    // -------------------------------------------------------------------------

    let mut display: Display<DcsState> = Display::new()?;

    // Wake the event loop whenever a connected client sends Wayland messages.
    // Without this source the loop only wakes on DRM VBlanks and new socket
    // connections, which means client messages are processed at VBlank rate
    // (or not at all once rendering is idle).
    // Duplicate the Wayland display poll fd into an OwnedFd so the mutable
    // borrow on `display.backend()` ends before we use `display` again.
    let wayland_poll_fd = display.backend().poll_fd().try_clone_to_owned()?;
    loop_handle.insert_source(
        Generic::new(wayland_poll_fd, Interest::READ, Mode::Level),
        |_, _, data: &mut DcsCalloopData| {
            data.display
                .dispatch_clients(&mut data.state)?;
            Ok(PostAction::Continue)
        },
    )?;

    // Advertise the DCS private protocol in the registry so clients can bind
    // the manager object.
    display
        .handle()
        .create_global::<DcsState, ZwpDcsManager, _>(1, ());

    // Advertise wp_drm_lease_device_v1 so Vulkan D2D clients can lease the
    // displays that DCS has configured.
    {
        let mut drm_lease_state = DrmLeaseState::new::<DcsState>(
            &display.handle(),
            &device.drm_node,
        )?;

        // Make every connected output available for leasing.
        for output in device.outputs.values() {
            let name = format!("DCS-{}", output.display_number);
            let desc = format!(
                "DCS display {} ({}x{})",
                output.display_number, output.mode_width, output.mode_height
            );
            drm_lease_state.add_connector::<DcsState>(output.connector_handle, name, desc);
        }

        device.drm_lease_state = Some(drm_lease_state);
    }

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
        state: DcsState { devices: vec![device], pending_commit: None },
        display,
    };

    info!("Display config server started");

    // The post-dispatch callback runs after each batch of events.  Render
    // here so any state change driven by events is reflected on screen, then
    // flush pending Wayland protocol messages back to clients.
    event_loop.run(None, &mut calloop_data, |data| {
        for device in &mut data.state.devices {
            device.render().expect("render failed");
        }
        data.display
            .dispatch_clients(&mut data.state)
            .expect("Error dispatching Wayland clients");
        data.display
            .flush_clients()
            .expect("Error flushing Wayland clients");
    })?;

    Ok(())
}
