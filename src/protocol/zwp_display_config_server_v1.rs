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
use drm::control::crtc;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::protocols::zwp_display_config_server_v1::{
    zwp_dcs_display_configuration::{self, Error as ConfigError, ZwpDcsDisplayConfiguration},
    zwp_dcs_manager::{self, ZwpDcsManager},
    zwp_dcs_output::{self, Mode as OutputMode, ZwpDcsOutput},
    zwp_dcs_topology::{self, Error as TopologyError, ZwpDcsTopology},
};
use crate::DcsState;

// ---------------------------------------------------------------------------
// zwp_dcs_manager
// ---------------------------------------------------------------------------

/// Per-resource state for a `zwp_dcs_output` protocol object.
///
/// Stores the `crtc::Handle` that links this protocol object back to the
/// corresponding [`crate::render::DcsOutput`] inside `DcsState::devices`.
/// `None` when the `wl_output` passed to `get_output` could not be resolved
/// to a known DCS CRTC (e.g. because the `wl_output` global is not yet
/// implemented).
pub struct WlDcsOutput {
    /// CRTC this output maps to in `DcsDevice::outputs`. `None` if the
    /// `wl_output` could not be resolved to a known DCS output.
    /// Used by topology commits to locate the backing `DcsOutput`.
    pub crtc: Option<crtc::Handle>,
}

/// Per-resource state for a `zwp_dcs_display_configuration` protocol object.
///
/// Carries the CRTC inherited from the [`WlDcsOutput`] it was created from,
/// plus any pending changes staged by `set_mode` / `set_number` requests.
/// The whole struct is wrapped in a [`Mutex`] as the user-data type, because
/// `Dispatch::request` receives `&UserData` (not `&mut`) and wayland-server
/// requires `UserData: Send + Sync`.
pub struct WlDcsDisplayConfiguration {
    /// CRTC inherited from the `WlDcsOutput` this configuration was created
    /// from. Identifies which display the pending changes apply to during `commit`.
    #[allow(dead_code)]
    pub crtc: Option<crtc::Handle>,
    /// Pending mode (width px, height px, refresh mHz) from `set_mode`.
    /// `None` means the mode is not being changed in this commit.
    pub pending_mode: Option<(u32, u32, u32)>,
    /// Pending display number from `set_number`.
    /// `None` means the display number is not being changed in this commit.
    pub pending_number: Option<u32>,
}

/// Per-resource state for a `zwp_dcs_topology` protocol object.
///
/// Accumulates [`ZwpDcsDisplayConfiguration`] resources as the client calls
/// `add_configuration`. The whole struct is wrapped in a [`Mutex`] as the
/// user-data type for the same reason as [`WlDcsDisplayConfiguration`].
pub struct WlDcsTopology {
    /// Configurations added via `add_configuration`, in insertion order.
    pub configurations: Vec<ZwpDcsDisplayConfiguration>,
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
                // Resolve the wl_output to a CRTC handle via its resource user data.
                // Returns None when wl_output globals are not yet implemented.
                let crtc = output.data::<crtc::Handle>().copied();
                if crtc.is_none() {
                    tracing::warn!("get_output: wl_output has no associated DCS CRTC");
                }

                let resource = data_init.init(id, WlDcsOutput { crtc });

                // Send the initial burst of events describing this output.
                if let Some(crtc) = crtc {
                    if let Some(dcs_out) = state.output_for_crtc(crtc) {
                        resource.mode(
                            OutputMode::Current,
                            dcs_out.mode_width,
                            dcs_out.mode_height,
                            dcs_out.mode_refresh_mhz,
                        );
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
                    }),
                );
            }
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
                        crtc: data.crtc,
                        pending_mode: None,
                        pending_number: None,
                    }),
                );
            }
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

                // Reject the configuration if its output is already represented
                // in this topology. Comparing by CRTC handle is sufficient
                // because each connected display maps to exactly one CRTC.
                let new_crtc = config
                    .data::<Mutex<WlDcsDisplayConfiguration>>()
                    .expect("wrong user data type on zwp_dcs_display_configuration")
                    .lock()
                    .unwrap()
                    .crtc;

                let duplicate = topology.configurations.iter().any(|existing| {
                    existing
                        .data::<Mutex<WlDcsDisplayConfiguration>>()
                        .expect("wrong user data type on zwp_dcs_display_configuration")
                        .lock()
                        .unwrap()
                        .crtc
                        == new_crtc
                });

                if duplicate {
                    resource.error(TopologyError::DuplicateConfigs);
                    return;
                }

                topology.configurations.push(config);
            }
            zwp_dcs_topology::Request::Commit => {
                let topology = data.lock().unwrap();
                if let Err((idx, e)) = state.apply_topology(&topology) {
                    tracing::error!("topology commit failed: {:#}", e);
                    topology.configurations[idx].error(ConfigError::InvalidState);
                    resource.error(TopologyError::Failed);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DcsState: topology application
// ---------------------------------------------------------------------------

impl DcsState {
    /// Apply every configuration in `topology` to the live display state.
    ///
    /// Processes each [`WlDcsDisplayConfiguration`] in insertion order:
    /// - **Mode changes** are test-committed atomically before being applied; a
    ///   hardware rejection is treated as a hard error.
    /// - **Number changes** are applied directly without hardware interaction.
    ///
    /// Returns `Ok(())` when all configurations applied successfully, or
    /// `Err((index, error))` identifying the first failing configuration. The
    /// caller is responsible for sending the corresponding protocol error events.
    fn apply_topology(&mut self, topology: &WlDcsTopology) -> Result<(), (usize, anyhow::Error)> {
        // Collect all pending changes up front, releasing config locks promptly.
        // Return early with the offending index if any config has no CRTC.
        let changes: Vec<(crtc::Handle, Option<(u32, u32, u32)>, Option<u32>)> = {
            let mut v = Vec::with_capacity(topology.configurations.len());
            for (idx, config_resource) in topology.configurations.iter().enumerate() {
                let config = config_resource
                    .data::<Mutex<WlDcsDisplayConfiguration>>()
                    .expect("wrong user data type on zwp_dcs_display_configuration")
                    .lock()
                    .unwrap();

                let Some(crtc) = config.crtc else {
                    return Err((idx, anyhow!("configuration has no associated CRTC")));
                };

                v.push((crtc, config.pending_mode, config.pending_number));
            }
            v
        };

        // Phase 1: validate all mode changes via atomic test commits before
        // applying anything, so a hardware rejection never partially applies.
        for (idx, &(crtc, pending_mode, _)) in changes.iter().enumerate() {
            // Test Mode
            if let Some((w, h, r)) = pending_mode {
                self.validate_output_mode_change(crtc, w, h, r)
                    .map_err(|e| (idx, e))?;
            }
        }

        // Phase 2: apply all changes now that every mode has been validated.
        for (idx, &(crtc, pending_mode, pending_number)) in changes.iter().enumerate() {
            // Apply Mode
            if let Some((w, h, r)) = pending_mode {
                self.commit_output_mode_change(crtc, w, h, r)
                    .map_err(|e| (idx, e))?;
            }

            // Update Number
            if let Some(number) = pending_number {
                if let Some(output) = self.output_for_crtc_mut(crtc) {
                    output.display_number = number as i32;
                    output.needs_render = true;
                }
            }
        }

        Ok(())
    }
}
