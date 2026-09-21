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
//! - [`WlDcsDisplayConfiguration`] — describes the desired state of one
//!   display (mode, ID number). Multiple configurations are bundled into a
//!   topology and committed atomically so the server can validate the full
//!   set of changes before applying any of them.
//!
//! - [`WlDcsTopology`] — groups one or more [`WlDcsDisplayConfiguration`]
//!   objects into a single atomic commit. On failure the topology and the
//!   offending configuration objects each post an error event.

use std::sync::Mutex;

use anyhow::anyhow;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::render::dcs_output::OutputHandle;

use crate::protocols::zwp_display_config_server_v1::{
    zwp_dcs_display_configuration::{self, Error as ConfigError, ZwpDcsDisplayConfiguration},
    zwp_dcs_manager::{self, ZwpDcsManager},
    zwp_dcs_output::{self, Mode as OutputMode, ZwpDcsOutput},
    zwp_dcs_topology::{self, Error as TopologyError, ZwpDcsTopology},
};
use crate::protocols::zwp_dcs_quadro_sync_v1::{
    zwp_dcs_quadro_sync_display_configuration::ZwpDcsQuadroSyncDisplayConfiguration,
    zwp_dcs_quadro_sync_topology::ZwpDcsQuadroSyncTopology,
};
use crate::DcsState;

// ---------------------------------------------------------------------------
// zwp_dcs_manager
// ---------------------------------------------------------------------------

/// Per-resource state for a `zwp_dcs_output` protocol object.
///
/// Stores the `OutputHandle` that links this protocol object back to the
/// corresponding [`crate::render::DcsOutput`] inside `DcsState::devices`.
/// `None` when the `wl_output` passed to `get_output` could not be resolved
/// to a known DCS output (e.g. because the `wl_output` global is not yet
/// implemented).
pub struct WlDcsOutput {
    /// Identifies the backing `DcsOutput` across all devices. `None` if the
    /// `wl_output` could not be resolved to a known DCS output.
    pub handle: Option<OutputHandle>,
}

/// Per-resource state for a `zwp_dcs_display_configuration` protocol object.
///
/// Carries the OutputHandle inherited from the [`WlDcsOutput`] it was created from,
/// plus any pending changes staged by `set_mode` / `set_number` requests.
/// The whole struct is wrapped in a [`Mutex`] as the user-data type, because
/// `Dispatch::request` receives `&UserData` (not `&mut`) and wayland-server
/// requires `UserData: Send + Sync`.
pub struct WlDcsDisplayConfiguration {
    /// Output inherited from the `WlDcsOutput` this configuration was created
    /// from. Identifies which display the pending changes apply to on `commit`.
    pub handle: Option<OutputHandle>,
    /// Pending mode (width px, height px, refresh mHz) from `set_mode`.
    /// `None` means the mode is not being changed in this commit.
    pub pending_mode: Option<(u32, u32, u32)>,
    /// Pending display number from `set_number`.
    /// `None` means the display number is not being changed in this commit.
    pub pending_number: Option<u32>,
    /// Optional QuadroSync display configuration extension, set when a client
    /// calls `zwp_dcs_quadro_sync_output.get_configuration(this_config)`.
    /// Collected at commit time so staged QuadroSync roles reach
    /// [`DcsState::build_quadro_sync_attribute`].
    pub quadro_sync_config: Option<ZwpDcsQuadroSyncDisplayConfiguration>,
}

/// Per-resource state for a `zwp_dcs_topology` protocol object.
///
/// Accumulates [`ZwpDcsDisplayConfiguration`] resources as the client calls
/// `add_configuration`. The whole struct is wrapped in a [`Mutex`] as the
/// user-data type for the same reason as [`WlDcsDisplayConfiguration`].
pub struct WlDcsTopology {
    /// Configurations added via `add_configuration`, in insertion order.
    pub configurations: Vec<ZwpDcsDisplayConfiguration>,
    /// Optional QuadroSync topology extension, set when a client calls
    /// `zwp_dcs_quadro_sync_manager.get_topology(this_topology)`.
    pub quadro_sync_topology: Option<ZwpDcsQuadroSyncTopology>,
}

impl WlDcsTopology {
    /// Forget any protocol objects the client has destroyed since we last
    /// looked. A destroyed configuration leaves the topology, and a destroyed
    /// QuadroSync extension no longer contributes to the commit, matching
    /// the `destroy` semantics in the protocol. Called before every use of
    /// the lists so a dead resource is never dereferenced.
    pub fn prune_dead(&mut self) {
        self.configurations.retain(|c| c.is_alive());
        if self.quadro_sync_topology.as_ref().is_some_and(|t| !t.is_alive()) {
            self.quadro_sync_topology = None;
        }
    }
}

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
                // Returns None when wl_output globals are not yet implemented.
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
                        // the EDID detailed timing block and the CEA section).
                        //   current (1)   = the active mode
                        //   preferred (2) = the first/native mode
                        //   none (0)      = any other available mode
                        let mut seen = std::collections::HashSet::new();
                        for (i, drm_mode) in dcs_out.connector_modes.iter().enumerate() {
                            let (w, h) = drm_mode.size();
                            let refresh_mhz = drm_mode.vrefresh() * 1000;
                            let key = (w as u32, h as u32, refresh_mhz);
                            if !seen.insert(key) {
                                continue;
                            }
                            let is_current = w as u32 == dcs_out.mode_width
                                && h as u32 == dcs_out.mode_height
                                && refresh_mhz == dcs_out.mode_refresh_mhz;
                            let flags = if is_current {
                                OutputMode::Current
                            } else if i == 0 {
                                OutputMode::Preferred
                            } else {
                                OutputMode::None
                            };
                            resource.mode(flags, w as u32, h as u32, refresh_mhz);
                        }
                        resource.device(dcs_out.dev_t);
                        resource.number(dcs_out.display_number);
                        resource.done();
                    }
                }
            }
            zwp_dcs_manager::Request::CreateTopology { id } => {
                data_init.init(
                    id,
                    Mutex::new(WlDcsTopology {
                        configurations: Vec::new(),
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
        data: &WlDcsOutput,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_output::Request::CreateConfiguration { id } => {
                data_init.init(
                    id,
                    Mutex::new(WlDcsDisplayConfiguration {
                        handle: data.handle,
                        pending_mode: None,
                        pending_number: None,
                        quadro_sync_config: None,
                    }),
                );
            }
            zwp_dcs_output::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_display_configuration
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsDisplayConfiguration, Mutex<WlDcsDisplayConfiguration>> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsDisplayConfiguration,
        request: zwp_dcs_display_configuration::Request,
        data: &Mutex<WlDcsDisplayConfiguration>,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut config = data.lock().unwrap();
        match request {
            zwp_dcs_display_configuration::Request::SetMode {
                width,
                height,
                refresh,
            } => {
                config.pending_mode = Some((width, height, refresh));
            }
            zwp_dcs_display_configuration::Request::SetNumber { number } => {
                config.pending_number = Some(number);
            }
            // A topology that holds this configuration drops it the next time
            // it looks at its list (see `WlDcsTopology::prune_dead`); the
            // pending state dies with the resource's user data.
            zwp_dcs_display_configuration::Request::Destroy => {}
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
        match request {
            zwp_dcs_topology::Request::AddConfiguration { config } => {
                let mut topology = data.lock().unwrap();
                topology.prune_dead();

                // Reject the configuration if its output is already represented
                // in this topology. Comparing by OutputHandle is sufficient
                // because each connected display maps to exactly one
                // (device, CRTC) pair.
                let new_handle = config
                    .data::<Mutex<WlDcsDisplayConfiguration>>()
                    .expect("wrong user data type on zwp_dcs_display_configuration")
                    .lock()
                    .unwrap()
                    .handle;

                let duplicate = topology.configurations.iter().any(|existing| {
                    existing
                        .data::<Mutex<WlDcsDisplayConfiguration>>()
                        .expect("wrong user data type on zwp_dcs_display_configuration")
                        .lock()
                        .unwrap()
                        .handle
                        == new_handle
                });

                if duplicate {
                    resource.error(TopologyError::DuplicateConfigs);
                    return;
                }

                topology.configurations.push(config);
            }
            zwp_dcs_topology::Request::Commit => {
                let mut topology = data.lock().unwrap();
                topology.prune_dead();
                if let Err((idx, e)) = state.apply_topology(&topology) {
                    tracing::error!("topology commit failed: {:#}", e);
                    topology.configurations[idx].error(ConfigError::InvalidState);
                    resource.error(TopologyError::Failed);
                }
            }
            // Configurations added here are separate objects the client still
            // owns; nothing to release.
            zwp_dcs_topology::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// DcsState: topology application
// ---------------------------------------------------------------------------

impl DcsState {
    /// Apply every configuration in `topology` to the live display state.
    ///
    /// Builds a [`PendingCommit`] from the protocol configurations and any
    /// sub-protocol topology extensions attached to them, then validates and
    /// applies all attributes atomically:
    ///
    /// 1. Validate topology attributes (cross-output invariants).
    /// 2. Validate standalone display attributes (mode tests, etc.).
    /// 3. Apply topology attributes (each applies its own child display attrs).
    /// 4. Apply standalone display attributes.
    ///
    /// Returns `Ok(())` when all attributes applied successfully, or
    /// `Err((index, error))` identifying the first failing configuration. The
    /// caller is responsible for sending the corresponding protocol error events.
    fn apply_topology(&mut self, topology: &WlDcsTopology) -> Result<(), (usize, anyhow::Error)> {
        use crate::attribute::{
            PendingCommit,
            display_number::DisplayNumberAttribute,
            mode::ModeAttribute,
        };

        let mut commit = PendingCommit::new();

        // If a QuadroSync topology extension is associated with this topology,
        // build its attribute and add it to the commit before processing the
        // base configurations. The QuadroSync display configurations are
        // reached through the base configurations registered on this topology:
        // each one that was extended via `get_configuration` carries its
        // QuadroSync companion in its user data.
        if let Some(ref qs_topo) = topology.quadro_sync_topology {
            let qs_configs: Vec<ZwpDcsQuadroSyncDisplayConfiguration> = topology
                .configurations
                .iter()
                .filter_map(|config| {
                    config
                        .data::<Mutex<WlDcsDisplayConfiguration>>()
                        .and_then(|d| d.lock().unwrap().quadro_sync_config.clone())
                })
                // A destroyed QuadroSync configuration leaves the role alone.
                .filter(|qs_config| qs_config.is_alive())
                .collect();
            let attr = self
                .build_quadro_sync_attribute(qs_topo, &qs_configs)
                .map_err(|e| (0, e))?;
            commit.topology_attrs.push(Box::new(attr));
        }

        for (idx, config_resource) in topology.configurations.iter().enumerate() {
            let config = config_resource
                .data::<Mutex<WlDcsDisplayConfiguration>>()
                .expect("wrong user data type on zwp_dcs_display_configuration")
                .lock()
                .unwrap();

            let Some(handle) = config.handle else {
                return Err((idx, anyhow!("configuration has no associated output")));
            };

            if let Some((w, h, r)) = config.pending_mode {
                commit.display_attrs.push((handle, Box::new(ModeAttribute {
                    width: w,
                    height: h,
                    refresh_mhz: r,
                })));
            }

            if let Some(number) = config.pending_number {
                commit.display_attrs.push((handle, Box::new(DisplayNumberAttribute {
                    number,
                })));
            }
        }

        // Phase 1: Validate topology attrs across all devices.
        for topo_attr in &commit.topology_attrs {
            topo_attr
                .validate(self)
                .map_err(|e| (0, e.context(format!("{} topology", topo_attr.name()))))?;
        }

        // Phase 2: Validate standalone display attrs.
        for (idx, (handle, attr)) in commit.display_attrs.iter().enumerate() {
            if let Some(output) = self.output_for_handle(*handle) {
                attr.validate(output).map_err(|e| (idx, e))?;
            } else {
                return Err((idx, anyhow!("no output found for {:?}", handle)));
            }
        }

        // Phase 3: Apply topology attrs (each routes to the devices it needs).
        for topo_attr in &commit.topology_attrs {
            topo_attr.apply(self).map_err(|e| (0, e))?;
        }

        // Phase 4: Apply standalone display attrs.
        for (idx, (handle, attr)) in commit.display_attrs.iter().enumerate() {
            if let Some(output) = self.output_for_handle_mut(*handle) {
                attr.apply(output).map_err(|e| (idx, e))?;
            }
        }

        Ok(())
    }
}
