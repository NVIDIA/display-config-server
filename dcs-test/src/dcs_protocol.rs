// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Client-side bindings for the DCS private Wayland protocol, and a helper
//! that connects to DCS and collects the advertised output state.

use wayland_client::{
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};

/// Client-side generated code for the DCS private protocol.
pub mod protocol {
    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    wayland_scanner::generate_interfaces!("../protocol/zwp_display_config_server_v1.xml");
    wayland_scanner::generate_client_code!("../protocol/zwp_display_config_server_v1.xml");
}

/// A mode reported by a DCS output.
#[allow(dead_code)] // fields used once wl_output globals are implemented
#[derive(Debug, Clone)]
pub struct DcsMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub current: bool,
}

/// State collected for one DCS output.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DcsOutputInfo {
    pub modes: Vec<DcsMode>,
    pub dev_t: Option<u32>,
    pub display_number: Option<i32>,
    pub done: bool,
}

/// All state collected from a DCS Wayland connection.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DcsProtocolState {
    /// Whether the zwp_dcs_manager global was found in the registry.
    pub manager_found: bool,
    /// Whether wp_drm_lease_device_v1 was found.
    pub drm_lease_found: bool,
    /// Outputs collected via get_output (requires wl_output globals).
    pub outputs: Vec<DcsOutputInfo>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for DcsProtocolState {
    fn event(
        state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            match interface.as_str() {
                "zwp_dcs_manager" => state.manager_found = true,
                "wp_drm_lease_device_v1" => state.drm_lease_found = true,
                _ => {}
            }
        }
    }
}

/// The default socket name used by DCS.
const DCS_SOCKET_NAME: &str = "display-config-server-0";

/// Connect to the DCS Wayland socket and query the advertised globals.
///
/// Connects to `display-config-server-0` by default. If `WAYLAND_DISPLAY`
/// is already set in the environment, that takes precedence.
pub fn query_dcs() -> Result<DcsProtocolState, Box<dyn std::error::Error>> {
    // Default to the DCS socket name if WAYLAND_DISPLAY is not set.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        std::env::set_var("WAYLAND_DISPLAY", DCS_SOCKET_NAME);
    }

    let conn = Connection::connect_to_env()?;
    let display = conn.display();

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = DcsProtocolState {
        manager_found: false,
        drm_lease_found: false,
        outputs: Vec::new(),
    };

    display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;

    Ok(state)
}
