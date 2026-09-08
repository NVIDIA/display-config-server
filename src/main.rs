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
mod wl_output;

use std::sync::Arc;

use render::DcsDevice;
use render::dcs_output::OutputHandle;
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
    /// Resolve an [`OutputHandle`] to its `DcsOutput`. Indexes the owning device
    /// directly; never scans other devices, so colliding per-device CRTC
    /// handles cannot resolve to the wrong GPU.
    fn output_for_handle(&self, handle: OutputHandle) -> Option<&render::dcs_output::DcsOutput> {
        self.devices.get(handle.device_index)?.outputs.get(&handle.crtc)
    }

    /// Mutable variant of [`output_for_handle`].
    fn output_for_handle_mut(&mut self, handle: OutputHandle) -> Option<&mut render::dcs_output::DcsOutput> {
        self.devices.get_mut(handle.device_index)?.outputs.get_mut(&handle.crtc)
    }

    /// The device owning `handle`, if the handle resolves to a known output.
    fn device_for_handle(&self, handle: OutputHandle) -> Option<&render::DcsDevice> {
        let device = self.devices.get(handle.device_index)?;
        device.outputs.contains_key(&handle.crtc).then_some(device)
    }

    /// Validate a mode change on the output identified by `handle`.
    fn validate_output_mode_change(
        &self,
        handle: OutputHandle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let device = self
            .device_for_handle(handle)
            .ok_or_else(|| anyhow::anyhow!("no device found for output {:?}", handle))?;
        device.validate_output_mode_change(handle.crtc, width, height, refresh_mhz)
    }

    /// Commit a mode change on the output identified by `handle`.
    fn commit_output_mode_change(
        &mut self,
        handle: OutputHandle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let device = self
            .devices
            .get_mut(handle.device_index)
            .filter(|d| d.outputs.contains_key(&handle.crtc))
            .ok_or_else(|| anyhow::anyhow!("no device found for output {:?}", handle))?;
        device.commit_output_mode_change(handle.crtc, width, height, refresh_mhz)
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
        node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        // The lease request arrives on a specific wp_drm_lease_device_v1
        // global, i.e. a specific GPU. Resolve connectors within that device
        // only; connector handles are not unique across GPUs.
        let device = self
            .devices
            .iter()
            .find(|d| d.drm_node == node)
            .filter(|d| {
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

    fn new_active_lease(&mut self, node: DrmNode, lease: DrmLease) {
        info!("DRM lease {} granted", lease.id());
        // Move the lease into the matching output. DrmLease::Drop revokes the
        // kernel lease, so we must move (not clone) it into persistent storage.
        let mut lease = Some(lease);
        if let Some(device) = self.devices.iter_mut().find(|d| d.drm_node == node) {
            for output in device.outputs.values_mut() {
                if lease.as_ref().map_or(false, |l| l.connectors().any(|c| *c == output.connector_handle)) {
                    output.active_lease = lease.take();
                    break;
                }
            }
        }
    }

    fn lease_destroyed(&mut self, node: DrmNode, lease_id: u32) {
        info!("DRM lease {} destroyed", lease_id);
        if let Some(device) = self.devices.iter_mut().find(|d| d.drm_node == node) {
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

/// Which DRM cards DCS should open.
#[derive(Debug)]
enum CardSelection {
    /// Open every `/dev/dri/card*` device.
    All,
    /// Open exactly this device path.
    Path(String),
}

/// Parse the command line (without argv[0]).
///
/// Supported forms:
///   (nothing)           →  All /dev/dri/card* devices
///   --card <N>          →  /dev/dri/cardN only
///   /dev/dri/cardN      →  that path only (positional)
fn parse_card_selection(args: &[String]) -> Result<CardSelection, String> {
    let mut iter = args.iter();
    let Some(arg) = iter.next() else {
        return Ok(CardSelection::All);
    };
    match arg.as_str() {
        "--card" => {
            let n = iter
                .next()
                .ok_or_else(|| "--card requires a card number argument".to_string())?;
            n.parse::<u32>()
                .map_err(|_| format!("--card: '{}' is not a valid card number", n))?;
            Ok(CardSelection::Path(format!("/dev/dri/card{}", n)))
        }
        path if !path.starts_with('-') => Ok(CardSelection::Path(path.to_string())),
        unknown => Err(format!("unknown argument: {}", unknown)),
    }
}

/// Every `/dev/dri/card*` primary node, sorted by card number so device
/// indices are stable across runs. Render nodes (`renderD*`) are excluded.
fn drm_card_paths() -> Vec<String> {
    let entries = match std::fs::read_dir("/dev/dri") {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!("cannot read /dev/dri: {}", e);
            return Vec::new();
        }
    };
    let mut cards: Vec<(u32, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(number_str) = name.strip_prefix("card") else {
            continue;
        };
        let Ok(number) = number_str.parse::<u32>() else {
            continue;
        };
        cards.push((number, format!("/dev/dri/{}", name)));
    }
    cards.sort_unstable_by_key(|(number, _)| *number);
    cards.into_iter().map(|(_, path)| path).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up tracing output. RUST_LOG controls the filter (e.g. RUST_LOG=debug).
    tracing_subscriber::fmt::init();

    // Parse --card <N> to select /dev/dri/cardN, or accept a full device path
    // as a positional argument. Defaults to opening every DRM card.
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    let selection = parse_card_selection(&cli_args)?;
    let explicit = matches!(selection, CardSelection::Path(_));
    let paths: Vec<String> = match selection {
        CardSelection::Path(path) => vec![path],
        CardSelection::All => {
            let found = drm_card_paths();
            if found.is_empty() {
                return Err("no DRM devices found under /dev/dri".into());
            }
            found
        }
    };

    // Open every selected card. With an explicit selection a failure is fatal;
    // when auto-enumerating, a GPU with no connected displays is skipped.
    let mut devices: Vec<DcsDevice> = Vec::new();
    let mut notifiers = Vec::new();
    let mut next_display_number: i32 = 1;
    for path in &paths {
        info!("Initialising DRM device: {}", path);
        match DcsDevice::new(path, next_display_number) {
            Ok((mut device, notifier)) => {
                device.render().expect("initial render failed");
                next_display_number += device.outputs.len() as i32;
                devices.push(device);
                notifiers.push(notifier);
            }
            Err(e) if explicit => return Err(e.into()),
            Err(e) => tracing::warn!("Skipping {}: {:#}", path, e),
        }
    }
    if devices.is_empty() {
        return Err("no usable DRM display devices found".into());
    }

    // -------------------------------------------------------------------------
    // Event loop
    // -------------------------------------------------------------------------

    let mut event_loop: EventLoop<DcsCalloopData> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    // VBlank events carry the CRTC handle so we can dispatch directly to the
    // output that flipped, leaving other outputs untouched.
    //
    // One VBlank source per device, each dispatching to its own device so a
    // flip on GPU 1 never touches GPU 0's outputs.
    for (device_index, drm_notifier) in notifiers.into_iter().enumerate() {
        loop_handle.insert_source(drm_notifier, move |event, _metadata, data| match event {
            DrmEvent::VBlank(crtc) => {
                data.state.devices[device_index]
                    .frame_submitted(crtc)
                    .expect("frame_submitted failed");
            }
            DrmEvent::Error(err) => {
                tracing::error!("DRM error on device {}: {}", device_index, err);
            }
        })?;
    }

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

    // Advertise the QuadroSync sub-protocol only when hardware is detected.
    if devices.iter().any(|d| d.quadro_sync_state.is_some()) {
        use crate::protocols::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_manager::ZwpDcsQuadroSyncManager;
        display
            .handle()
            .create_global::<DcsState, ZwpDcsQuadroSyncManager, _>(1, ());
        tracing::info!("Advertising zwp_dcs_quadro_sync_manager global");
    }

    // Advertise wp_drm_lease_device_v1 so Vulkan D2D clients can lease the
    // displays that DCS has configured.
    //
    // One wp_drm_lease_device_v1 global per GPU, each advertising only that
    // GPU's connectors.
    for device in &mut devices {
        let mut drm_lease_state = DrmLeaseState::new::<DcsState>(&display.handle(), &device.drm_node)?;
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

    // Advertise one wl_output global per display across all devices, so that
    // dcs-client clients can call zwp_dcs_manager.get_output().
    // The output's OutputHandle is stored as global data and resolved by the
    // get_output handler via output.data::<OutputHandle>().
    {
        use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
        for (device_index, device) in devices.iter().enumerate() {
            for &crtc in device.outputs.keys() {
                let handle = OutputHandle::new(device_index, crtc);
                display.handle().create_global::<DcsState, WlOutput, _>(4, handle);
            }
        }
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
        state: DcsState { devices, pending_commit: None },
        display,
    };

    info!("Display config server started");

    // The post-dispatch callback runs after each batch of events.  Dispatch
    // client messages first so that any state changes (mode commits, etc.) are
    // applied before rendering, ensuring the new configuration is reflected on
    // screen in the same iteration rather than the next one.
    event_loop.run(None, &mut calloop_data, |data| {
        data.display
            .dispatch_clients(&mut data.state)
            .expect("Error dispatching Wayland clients");
        for device in &mut data.state.devices {
            device.render().expect("render failed");
        }
        data.display
            .flush_clients()
            .expect("Error flushing Wayland clients");
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_selects_all_cards() {
        assert!(matches!(parse_card_selection(&args(&[])).unwrap(), CardSelection::All));
    }

    #[test]
    fn card_flag_selects_one_path() {
        match parse_card_selection(&args(&["--card", "1"])).unwrap() {
            CardSelection::Path(p) => assert_eq!(p, "/dev/dri/card1"),
            other => panic!("expected Path, got {:?}", other),
        }
    }

    #[test]
    fn positional_path_selects_that_path() {
        match parse_card_selection(&args(&["/dev/dri/card3"])).unwrap() {
            CardSelection::Path(p) => assert_eq!(p, "/dev/dri/card3"),
            other => panic!("expected Path, got {:?}", other),
        }
    }

    #[test]
    fn card_flag_requires_value() {
        assert!(parse_card_selection(&args(&["--card"])).is_err());
    }

    #[test]
    fn card_flag_rejects_non_numeric() {
        assert!(parse_card_selection(&args(&["--card", "zero"])).is_err());
    }

    #[test]
    fn unknown_flag_rejected() {
        assert!(parse_card_selection(&args(&["--bogus"])).is_err());
    }
}
