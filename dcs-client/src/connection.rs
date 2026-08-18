// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Wayland connection management for DCS clients.

use anyhow::Context;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    protocol::{wl_output::WlOutput, wl_registry},
};

use crate::output::{ModeInfo, OutputInfo};
use crate::protocol::zwp_display_config_server_v1::{
    zwp_dcs_display_configuration::{self, ZwpDcsDisplayConfiguration},
    zwp_dcs_manager::{self, ZwpDcsManager},
    zwp_dcs_output::{self, ZwpDcsOutput},
    zwp_dcs_topology::{self, ZwpDcsTopology},
};
use crate::protocol::zwp_dcs_quadro_sync_v1::{
    zwp_dcs_quadro_sync_display_configuration::{
        self, ZwpDcsQuadroSyncDisplayConfiguration,
    },
    zwp_dcs_quadro_sync_manager::{self, ZwpDcsQuadroSyncManager},
    zwp_dcs_quadro_sync_output::{self, ZwpDcsQuadroSyncOutput},
    zwp_dcs_quadro_sync_topology::{self, ZwpDcsQuadroSyncTopology},
};

/// The default DCS Wayland socket name.
const DCS_SOCKET: &str = "display-config-server-0";

// ---------------------------------------------------------------------------
// Internal state types
// ---------------------------------------------------------------------------

/// Output being collected during an `enumerate_outputs` roundtrip.
#[derive(Default, Clone)]
pub(crate) struct PendingOutput {
    pub display_number: Option<i32>,
    pub dev_t: Option<u32>,
    pub modes: Vec<ModeInfo>,
    pub done: bool,
    /// QuadroSync role raw value (0=disabled, 1=server, 2=client), if reported.
    pub qs_role: Option<u32>,
    /// Whether QuadroSync sync is currently active, if reported.
    pub qs_sync_active: Option<bool>,
}

/// A fully-resolved output: the raw `wl_output` proxy (needed for
/// subsequent `get_output` calls in `apply`) paired with its current info.
#[derive(Clone)]
pub(crate) struct BoundOutput {
    pub wl_output: WlOutput,
    pub info: OutputInfo,
}

/// Per-event-queue mutable state shared across all dispatch impls.
pub(crate) struct ClientState {
    /// Bound during the registry roundtrip in `connect()`.
    pub manager_raw: Option<ZwpDcsManager>,
    /// Raw `wl_output` globals collected from the registry.
    pub wl_outputs: Vec<WlOutput>,
    /// Set to true if `wp_drm_lease_device_v1` is advertised.
    pub drm_lease_found: bool,
    /// Bound if `zwp_dcs_quadro_sync_manager` is advertised (QuadroSync
    /// hardware detected by DCS).
    pub quadro_sync_manager: Option<ZwpDcsQuadroSyncManager>,
    /// Temporary output slots, indexed by position in `wl_outputs`,
    /// populated during `enumerate_outputs`.
    pub pending: Vec<PendingOutput>,
    /// Fully resolved outputs; set at the end of `enumerate_outputs`.
    pub bound_outputs: Vec<BoundOutput>,
    /// Set to true if a topology commit error event is received.
    pub topology_error: bool,
    /// Set to true if a display configuration error event is received.
    pub config_error: bool,
}

// ---------------------------------------------------------------------------
// Public client handle
// ---------------------------------------------------------------------------

/// A live connection to a running DCS instance.
///
/// Obtained via [`connect`].  All protocol operations go through this type.
pub struct DcsClient {
    /// Bound `zwp_dcs_manager` — kept separate from `state` to allow
    /// borrowing `manager` and `&mut state` simultaneously.
    pub(crate) manager: ZwpDcsManager,
    pub(crate) state: ClientState,
    pub(crate) event_queue: EventQueue<ClientState>,
    pub(crate) qh: QueueHandle<ClientState>,
}

impl DcsClient {
    /// Returns true if `wp_drm_lease_device_v1` was advertised by the server.
    pub fn drm_lease_found(&self) -> bool {
        self.state.drm_lease_found
    }

    /// Returns true if QuadroSync hardware was detected by DCS.
    pub fn quadro_sync_supported(&self) -> bool {
        self.state.quadro_sync_manager.is_some()
    }
}

/// Connect to the DCS Wayland socket.
///
/// Uses `$WAYLAND_DISPLAY` if set; otherwise defaults to
/// `display-config-server-0`.  Returns `Err` if DCS is not running or if
/// `zwp_dcs_manager` is not in the registry.
pub fn connect() -> anyhow::Result<DcsClient> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        // Safety: single-threaded at this point; no other thread reads env.
        unsafe { std::env::set_var("WAYLAND_DISPLAY", DCS_SOCKET) };
    }

    let conn = Connection::connect_to_env()
        .context("failed to connect to DCS socket — is DCS running?")?;

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = ClientState {
        manager_raw: None,
        wl_outputs: Vec::new(),
        drm_lease_found: false,
        quadro_sync_manager: None,
        pending: Vec::new(),
        bound_outputs: Vec::new(),
        topology_error: false,
        config_error: false,
    };

    conn.display().get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip failed")?;

    let manager = state
        .manager_raw
        .take()
        .context("zwp_dcs_manager not found — is this a DCS socket?")?;

    Ok(DcsClient { manager, state, event_queue, qh })
}

// ---------------------------------------------------------------------------
// Dispatch implementations
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwp_dcs_manager" => {
                    let mgr: ZwpDcsManager = registry.bind(name, 1, qh, ());
                    state.manager_raw = Some(mgr);
                }
                "wl_output" => {
                    let output: WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.wl_outputs.push(output);
                }
                "wp_drm_lease_device_v1" => {
                    state.drm_lease_found = true;
                }
                "zwp_dcs_quadro_sync_manager" => {
                    let mgr: ZwpDcsQuadroSyncManager = registry.bind(name, 1, qh, ());
                    state.quadro_sync_manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwpDcsManager, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsManager,
        _event: zwp_dcs_manager::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_dcs_manager has no events in v1.
    }
}

impl Dispatch<WlOutput, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &WlOutput,
        _event: wayland_client::protocol::wl_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Output info is collected via ZwpDcsOutput events, not wl_output events.
    }
}

/// User data = index into `state.pending`; populated during `enumerate_outputs`.
impl Dispatch<ZwpDcsOutput, usize> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpDcsOutput,
        event: zwp_dcs_output::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(pending) = state.pending.get_mut(*index) else { return };
        match event {
            zwp_dcs_output::Event::Mode { mode, width, height, refresh } => {
                // mode enum: 0 = none, 1 = current (active), 2 = preferred (native).
                let raw: u32 = mode.into();
                pending.modes.push(ModeInfo {
                    width,
                    height,
                    refresh_mhz: refresh,
                    current:   raw == 1,
                    preferred: raw == 2,
                });
            }
            zwp_dcs_output::Event::Device { device } => {
                pending.dev_t = Some(device);
            }
            zwp_dcs_output::Event::Number { display_number } => {
                pending.display_number = Some(display_number);
            }
            zwp_dcs_output::Event::Done => {
                pending.done = true;
            }
        }
    }
}

/// User data = `()` for ZwpDcsOutput created during `apply` (events ignored).
impl Dispatch<ZwpDcsOutput, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsOutput,
        _event: zwp_dcs_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Events from outputs created in apply() are not needed.
    }
}

impl Dispatch<ZwpDcsTopology, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpDcsTopology,
        event: zwp_dcs_topology::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_dcs_topology::Event::Error { .. } => {
                state.topology_error = true;
            }
        }
    }
}

impl Dispatch<ZwpDcsDisplayConfiguration, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpDcsDisplayConfiguration,
        event: zwp_dcs_display_configuration::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_dcs_display_configuration::Event::Error { .. } => {
                state.config_error = true;
            }
        }
    }
}

impl Dispatch<ZwpDcsQuadroSyncManager, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsQuadroSyncManager,
        _event: zwp_dcs_quadro_sync_manager::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_dcs_quadro_sync_manager has no events in v1.
    }
}

/// User data = index into `state.pending`; populated during `enumerate_outputs`.
impl Dispatch<ZwpDcsQuadroSyncOutput, usize> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpDcsQuadroSyncOutput,
        event: zwp_dcs_quadro_sync_output::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(pending) = state.pending.get_mut(*index) else { return };
        match event {
            zwp_dcs_quadro_sync_output::Event::SyncStatus { enabled } => {
                pending.qs_sync_active = Some(enabled != 0);
            }
            zwp_dcs_quadro_sync_output::Event::Role { role } => {
                pending.qs_role = Some(role.into());
            }
            zwp_dcs_quadro_sync_output::Event::Done => {}
        }
    }
}

/// User data = `()` for QuadroSync outputs created during `apply` (events ignored).
impl Dispatch<ZwpDcsQuadroSyncOutput, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsQuadroSyncOutput,
        _event: zwp_dcs_quadro_sync_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Events from QuadroSync outputs created in apply() are not needed.
    }
}

impl Dispatch<ZwpDcsQuadroSyncDisplayConfiguration, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsQuadroSyncDisplayConfiguration,
        _event: zwp_dcs_quadro_sync_display_configuration::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_dcs_quadro_sync_display_configuration has no events in v1.
        // Commit errors surface through the base protocol's error events.
    }
}

impl Dispatch<ZwpDcsQuadroSyncTopology, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _: &ZwpDcsQuadroSyncTopology,
        _event: zwp_dcs_quadro_sync_topology::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_dcs_quadro_sync_topology has no events in v1.
        // Commit errors surface through the base protocol's error events.
    }
}
