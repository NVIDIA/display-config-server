// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Server-side request handlers for the `zwp_display_config_server_v1` private
//! Wayland protocol.
//!
//! This protocol is the IPC channel between DCS and the Dynamic Configuration
//! Tool (`nvdisplayconf`). It is intentionally kept separate from the upstream
//! `wp_drm_lease_device_v1` protocol, which handles the actual transfer of DRM
//! display ownership to Vulkan D2D clients.
//!
//! # Protocol objects
//!
//! - [`WlDcsManager`] — global singleton advertised in `wl_registry`. Clients
//!   bind it to obtain per-output companion objects and to create topology
//!   objects for atomic multi-display commits.
//!
//! - [`WlDcsOutput`] — extends a `wl_output` with DCS-specific state: the
//!   display's ID number (shown on the splash screen), supported modes, and
//!   the underlying DRM device.
//!
//! - [`WlDcsTopology`] — stages per-display changes, each request naming the
//!   `wl_output` it applies to, and commits them atomically. Sub-protocol
//!   topology objects (QuadroSync) hang off it and are applied by the same
//!   commit. On failure the topology posts one `error` per offending display
//!   followed by a terminating `error` with a null output.

use std::sync::Mutex;

use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::render::dcs_output::OutputHandle;

use crate::protocols::zwp_display_config_server_v1::{
    zwp_dcs_manager::{self, ZwpDcsManager},
    zwp_dcs_output::{self, ZwpDcsOutput},
    zwp_dcs_topology::{self, Error as TopologyError, ZwpDcsTopology},
};
use crate::protocols::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_topology::{
    Error as QuadroSyncError, ZwpDcsQuadroSyncTopology,
};
use crate::DcsState;

// ---------------------------------------------------------------------------
// User data types
// ---------------------------------------------------------------------------

/// Per-resource state for a `zwp_dcs_output` protocol object.
///
/// Stores the `OutputHandle` that links this protocol object back to the
/// corresponding [`crate::render::DcsOutput`] inside `DcsState::devices`.
/// `None` when the `wl_output` passed to `get_output` could not be resolved
/// to a known DCS output.
pub struct WlDcsOutput {
    /// Identifies the backing `DcsOutput` across all devices. `None` if the
    /// `wl_output` could not be resolved to a known DCS output.
    pub handle: Option<OutputHandle>,
}

/// State staged for one display by `set_mode` / `set_number`.
pub struct PendingDisplay {
    /// The `wl_output` the client named. Kept so commit errors can point at it.
    pub output: WlOutput,
    /// Resolved from the `wl_output`'s user data when the first request for
    /// this display arrived. `None` means the client handed us a `wl_output`
    /// DCS does not manage; reported as `unknown_output` at commit.
    pub handle: Option<OutputHandle>,
    /// Opaque mode id from a `zwp_dcs_output.mode` event (see [`mode_id`]).
    pub mode_id: Option<u32>,
    /// New display ID number.
    pub number: Option<u32>,
}

/// Per-resource state for a `zwp_dcs_topology` protocol object.
///
/// Wrapped in a [`Mutex`] as the user-data type because `Dispatch::request`
/// receives `&UserData` (not `&mut`) and wayland-server requires
/// `UserData: Send + Sync`.
pub struct WlDcsTopology {
    /// Displays with staged changes, in the order the client first named them.
    pub displays: Vec<PendingDisplay>,
    /// Optional QuadroSync topology extension, set when a client calls
    /// `zwp_dcs_quadro_sync_manager.get_topology(this_topology)`.
    pub quadro_sync_topology: Option<ZwpDcsQuadroSyncTopology>,
}

impl WlDcsTopology {
    /// The staging entry for `output`, created on first use.
    fn display_mut(&mut self, output: &WlOutput) -> &mut PendingDisplay {
        if let Some(i) = self.displays.iter().position(|d| d.output == *output) {
            return &mut self.displays[i];
        }
        self.displays.push(PendingDisplay {
            output: output.clone(),
            handle: output.data::<OutputHandle>().copied(),
            mode_id: None,
            number: None,
        });
        self.displays.last_mut().unwrap()
    }

    /// Forget any protocol objects the client has destroyed since we last
    /// looked: a released `wl_output` drops its staged state, and a destroyed
    /// QuadroSync extension no longer contributes to the commit. Called before
    /// every use of the lists so a dead resource is never dereferenced.
    pub fn prune_dead(&mut self) {
        self.displays.retain(|d| d.output.is_alive());
        if self.quadro_sync_topology.as_ref().is_some_and(|t| !t.is_alive()) {
            self.quadro_sync_topology = None;
        }
    }
}

/// One problem found while committing a topology.
///
/// `apply_topology` collects these instead of stopping at the first failure,
/// so the client hears about every display at fault in one reply. The
/// `Commit` handler turns each one into an `error` event on the right
/// protocol object.
pub enum CommitFailure {
    /// A display's staged base state was rejected.
    Display { output: WlOutput, error: TopologyError },
    /// A display's staged QuadroSync role was rejected.
    QuadroSyncDisplay { output: WlOutput, error: QuadroSyncError },
    /// The QuadroSync settings were rejected as a whole (e.g. role counts).
    QuadroSync,
}

/// The opaque mode id advertised for `connector_modes[index]`.
///
/// Internally the id is just the index into the output's DRM mode list. The
/// protocol keeps it opaque so the server is free to change this later, but
/// the same scheme must be used by everything that mints or resolves ids:
/// the `mode`/`current_mode`/`preferred_mode` events here and the `set_mode`
/// resolution in [`DcsState::apply_topology`].
fn mode_id(index: usize) -> u32 {
    index as u32
}

// ---------------------------------------------------------------------------
// zwp_dcs_manager
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZwpDcsManager, ()> for DcsState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpDcsManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpDcsManager, ()> for DcsState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsManager,
        request: zwp_dcs_manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_manager::Request::GetOutput { id, output } => {
                // Resolve the wl_output to an OutputHandle via its resource user data.
                let handle = output.data::<OutputHandle>().copied();
                if handle.is_none() {
                    tracing::warn!("get_output: wl_output has no associated DCS output");
                }

                let resource = data_init.init(id, WlDcsOutput { handle });

                // Send the initial burst of events describing this output.
                if let Some(handle) = handle {
                    if let Some(dcs_out) = state.output_for_handle(handle) {
                        // Send one mode event per unique (width, height, refresh)
                        // combination.  DRM connector mode lists can contain
                        // duplicate entries (e.g. the same mode appearing in both
                        // the EDID detailed timing block and the CEA section);
                        // the first occurrence's index becomes the mode id.
                        // The active mode is whichever advertised entry matches
                        // the output's current mode, and the preferred mode is
                        // the first-listed one, as with DRM.
                        let mut seen = std::collections::HashSet::new();
                        let mut current_id = None;
                        let mut preferred_id = None;
                        for (i, drm_mode) in dcs_out.connector_modes.iter().enumerate() {
                            let (w, h) = drm_mode.size();
                            let refresh_mhz = drm_mode.vrefresh() * 1000;
                            let key = (w as u32, h as u32, refresh_mhz);
                            if !seen.insert(key) {
                                continue;
                            }
                            let id = mode_id(i);
                            resource.mode(id, w as u32, h as u32, refresh_mhz);

                            if preferred_id.is_none() {
                                preferred_id = Some(id);
                            }
                            if current_id.is_none()
                                && w as u32 == dcs_out.mode_width
                                && h as u32 == dcs_out.mode_height
                                && refresh_mhz == dcs_out.mode_refresh_mhz
                            {
                                current_id = Some(id);
                            }
                        }
                        if let Some(id) = current_id {
                            resource.current_mode(id);
                        }
                        if let Some(id) = preferred_id {
                            resource.preferred_mode(id);
                        }
                        // dev_t goes over the wire as sizeof(dev_t) native-endian
                        // bytes, matching wp_linux_dmabuf_feedback.main_device.
                        resource.device(dcs_out.dev_t.to_ne_bytes().to_vec());
                        resource.number(dcs_out.display_number);
                        resource.done();
                    }
                }
            }
            zwp_dcs_manager::Request::CreateTopology { id } => {
                data_init.init(
                    id,
                    Mutex::new(WlDcsTopology {
                        displays: Vec::new(),
                        quadro_sync_topology: None,
                    }),
                );
            }
            // Child objects stay valid; there is no manager-level state to drop.
            zwp_dcs_manager::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_output
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsOutput, WlDcsOutput> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsOutput,
        request: zwp_dcs_output::Request,
        _data: &WlDcsOutput,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_output::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_topology
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsTopology, Mutex<WlDcsTopology>> for DcsState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpDcsTopology,
        request: zwp_dcs_topology::Request,
        data: &Mutex<WlDcsTopology>,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut topology = data.lock().unwrap();
        topology.prune_dead();

        match request {
            // Staging requests only record values. An unknown wl_output is
            // staged too, so it is reported through the commit reply like
            // every other problem rather than at request time.
            zwp_dcs_topology::Request::SetMode { output, id } => {
                topology.display_mut(&output).mode_id = Some(id);
            }
            zwp_dcs_topology::Request::SetNumber { output, number } => {
                topology.display_mut(&output).number = Some(number);
            }
            zwp_dcs_topology::Request::Commit => {
                // Every commit gets exactly one terminator: done, or error
                // with a null output. Per-display and sub-protocol errors
                // come first.
                match state.apply_topology(&topology) {
                    Ok(()) => resource.done(),
                    Err(failures) => {
                        let qs_topo = topology.quadro_sync_topology.as_ref();
                        for failure in failures {
                            match failure {
                                CommitFailure::Display { output, error } => {
                                    resource.error(Some(&output), error);
                                }
                                CommitFailure::QuadroSyncDisplay { output, error } => {
                                    if let Some(qs) = qs_topo {
                                        qs.error(Some(&output), error);
                                    }
                                }
                                CommitFailure::QuadroSync => {
                                    if let Some(qs) = qs_topo {
                                        qs.error(None, QuadroSyncError::Failed);
                                    }
                                }
                            }
                        }
                        resource.error(None, TopologyError::Failed);
                    }
                }
            }
            // Staged state dies with the user data; sub-protocol objects are
            // separate resources the client still owns.
            zwp_dcs_topology::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// DcsState: topology application
// ---------------------------------------------------------------------------

impl DcsState {
    /// Apply every staged change in `topology` to the live display state.
    ///
    /// Builds a [`PendingCommit`] from the staged per-display state and any
    /// sub-protocol topology extension, then validates and applies all
    /// attributes atomically:
    ///
    /// 1. Validate topology attributes (cross-output invariants).
    /// 2. Validate standalone display attributes (mode tests, etc.).
    /// 3. Apply topology attributes (each applies its own child display attrs).
    /// 4. Apply standalone display attributes.
    ///
    /// Validation is side-effect-free, so every staged display and attribute
    /// is checked and all problems are collected before anything is applied.
    /// Returns `Ok(())` when everything applied, or the full list of
    /// [`CommitFailure`]s. The caller sends the corresponding protocol events.
    fn apply_topology(&mut self, topology: &WlDcsTopology) -> Result<(), Vec<CommitFailure>> {
        use crate::attribute::{
            PendingCommit,
            display_number::DisplayNumberAttribute,
            mode::ModeAttribute,
        };

        let mut commit = PendingCommit::new();
        let mut failures = Vec::new();

        // The QuadroSync extension stages its own per-display roles; turn
        // them into the topology attribute the commit phases understand.
        if let Some(qs_topo) = &topology.quadro_sync_topology {
            match self.build_quadro_sync_attribute(qs_topo) {
                Ok(Some(attr)) => commit.topology_attrs.push(Box::new(attr)),
                Ok(None) => {}
                Err(qs_failures) => failures.extend(qs_failures),
            }
        }

        // Per-display base state. `wl_output` lookups for error reporting go
        // through this list, so keep it in the client's order.
        for display in &topology.displays {
            let Some(handle) = display.handle else {
                failures.push(CommitFailure::Display {
                    output: display.output.clone(),
                    error: TopologyError::UnknownOutput,
                });
                continue;
            };
            let Some(output) = self.output_for_handle(handle) else {
                failures.push(CommitFailure::Display {
                    output: display.output.clone(),
                    error: TopologyError::UnknownOutput,
                });
                continue;
            };

            if let Some(id) = display.mode_id {
                // Resolve the opaque id back to the DRM mode it was minted from.
                let drm_mode = output
                    .connector_modes
                    .iter()
                    .enumerate()
                    .find(|(i, _)| mode_id(*i) == id)
                    .map(|(_, m)| m);
                match drm_mode {
                    Some(m) => {
                        let (w, h) = m.size();
                        commit.display_attrs.push((handle, Box::new(ModeAttribute {
                            width: w as u32,
                            height: h as u32,
                            refresh_mhz: m.vrefresh() * 1000,
                        })));
                    }
                    None => failures.push(CommitFailure::Display {
                        output: display.output.clone(),
                        error: TopologyError::InvalidMode,
                    }),
                }
            }

            if let Some(number) = display.number {
                commit.display_attrs.push((handle, Box::new(DisplayNumberAttribute {
                    number,
                })));
            }
        }

        // Resolve an OutputHandle back to the wl_output the client used, for
        // attribute failures below.
        let wl_output_for = |handle: OutputHandle| -> Option<WlOutput> {
            topology
                .displays
                .iter()
                .find(|d| d.handle == Some(handle))
                .map(|d| d.output.clone())
        };

        // Phase 1: Validate topology attrs across all devices.
        for topo_attr in &commit.topology_attrs {
            if let Err(e) = topo_attr.validate(self) {
                tracing::error!("{} topology rejected: {:#}", topo_attr.name(), e);
                failures.push(CommitFailure::QuadroSync);
            }
        }

        // Phase 2: Validate standalone display attrs.
        for (handle, attr) in &commit.display_attrs {
            let Some(output) = self.output_for_handle(*handle) else {
                continue; // already reported as unknown_output above
            };
            if let Err(e) = attr.validate(output) {
                tracing::error!("{} rejected for {:?}: {:#}", attr.name(), handle, e);
                if let Some(wl_output) = wl_output_for(*handle) {
                    failures.push(CommitFailure::Display {
                        output: wl_output,
                        error: TopologyError::InvalidState,
                    });
                }
            }
        }

        // Nothing is applied unless everything validated.
        if !failures.is_empty() {
            return Err(failures);
        }

        // Phase 3: Apply topology attrs (each routes to the devices it needs).
        for topo_attr in &commit.topology_attrs {
            if let Err(e) = topo_attr.apply(self) {
                tracing::error!("{} topology apply failed: {:#}", topo_attr.name(), e);
                failures.push(CommitFailure::QuadroSync);
            }
        }

        // Phase 4: Apply standalone display attrs.
        for (handle, attr) in &commit.display_attrs {
            let Some(output) = self.output_for_handle_mut(*handle) else {
                continue;
            };
            if let Err(e) = attr.apply(output) {
                tracing::error!("{} apply failed for {:?}: {:#}", attr.name(), handle, e);
                if let Some(wl_output) = wl_output_for(*handle) {
                    failures.push(CommitFailure::Display {
                        output: wl_output,
                        error: TopologyError::InvalidState,
                    });
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }
}
