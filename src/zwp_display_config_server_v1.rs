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
//! - [`ZwpDcsManager`] — global singleton advertised in `wl_registry`. Clients
//!   bind it to obtain per-output companion objects and to create topology
//!   objects for atomic multi-display commits.
//!
//! - [`ZwpDcsOutput`] — extends a `wl_output` with DCS-specific state: the
//!   display's ID number (shown on the splash screen), supported modes, and
//!   the underlying DRM device.
//!
//! - [`ZwpDcsDisplayConfiguration`] — describes the desired state of one
//!   display (mode, ID number). Multiple configurations are bundled into a
//!   topology and committed atomically so the server can validate the full
//!   set of changes before applying any of them.
//!
//! - [`ZwpDcsTopology`] — groups one or more [`ZwpDcsDisplayConfiguration`]
//!   objects into a single atomic commit. On failure the topology and the
//!   offending configuration objects each post an error event.

use wayland_server::{Client, DataInit, DisplayHandle, Dispatch, GlobalDispatch, New};

use crate::protocols::zwp_display_config_server_v1::{
    zwp_dcs_display_configuration::{self, ZwpDcsDisplayConfiguration},
    zwp_dcs_manager::{self, ZwpDcsManager},
    zwp_dcs_output::{self, ZwpDcsOutput},
    zwp_dcs_topology::{self, ZwpDcsTopology},
};
use crate::DcsState;

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
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsManager,
        request: zwp_dcs_manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_manager::Request::GetOutput { id, output: _ } => {
                data_init.init(id, ());
            }
            zwp_dcs_manager::Request::CreateTopology { id } => {
                data_init.init(id, ());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_output
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsOutput, ()> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsOutput,
        request: zwp_dcs_output::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_output::Request::CreateConfiguration { id } => {
                data_init.init(id, ());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_display_configuration
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsDisplayConfiguration, ()> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsDisplayConfiguration,
        request: zwp_dcs_display_configuration::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_display_configuration::Request::SetMode { .. } => {}
            zwp_dcs_display_configuration::Request::SetNumber { .. } => {}
        }
    }
}

// ---------------------------------------------------------------------------
// zwp_dcs_topology
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsTopology, ()> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsTopology,
        request: zwp_dcs_topology::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_topology::Request::AddConfiguration { .. } => {}
            zwp_dcs_topology::Request::Commit => {}
        }
    }
}
