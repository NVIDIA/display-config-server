// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
/// Generated server-side bindings for the DCS private Wayland protocol.
pub mod zwp_display_config_server_v1 {
    use wayland_server;
    use wayland_server::backend as wayland_backend;
    use wayland_server::protocol::wl_output;
    use wayland_server::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    wayland_scanner::generate_interfaces!("protocol/zwp_display_config_server_v1.xml");
    wayland_scanner::generate_server_code!("protocol/zwp_display_config_server_v1.xml");
}

/// Generated server-side bindings for the QuadroSync sub-protocol.
pub mod zwp_dcs_quadro_sync_v1 {
    use wayland_server;
    use wayland_server::backend as wayland_backend;
    use wayland_server::protocol::wl_output;
    use wayland_server::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    // Re-export the base protocol interface modules so that generate_server_code!
    // can reference `super::zwp_dcs_output::ZwpDcsOutput` etc. in its generated code.
    pub use super::zwp_display_config_server_v1::zwp_dcs_output;
    pub use super::zwp_display_config_server_v1::zwp_dcs_topology;

    // Re-export base protocol INTERFACE constants (SCREAMING_SNAKE_CASE from
    // generate_interfaces! and snake_case C statics from c_interfaces generation).
    pub use super::zwp_display_config_server_v1::{
        ZWP_DCS_OUTPUT_INTERFACE,
        ZWP_DCS_TOPOLOGY_INTERFACE,
        zwp_dcs_output_interface,
        zwp_dcs_topology_interface,
    };

    wayland_scanner::generate_interfaces!("protocol/zwp_dcs_quadro_sync_v1.xml");
    wayland_scanner::generate_server_code!("protocol/zwp_dcs_quadro_sync_v1.xml");
}
