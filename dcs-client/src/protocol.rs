// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Generated client-side bindings for the DCS private Wayland protocol.
//!
//! Re-exported from `dcs_client::protocol` for use by connection and apply
//! modules.  The path `"../protocol/..."` is relative to the crate root.

pub mod zwp_display_config_server_v1 {
    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    wayland_scanner::generate_interfaces!(
        "../protocol/zwp_display_config_server_v1.xml"
    );
    wayland_scanner::generate_client_code!(
        "../protocol/zwp_display_config_server_v1.xml"
    );
}

pub mod zwp_dcs_quadro_sync_v1 {
    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    // Re-export the base protocol interface modules so that
    // generate_client_code! can reference `super::zwp_dcs_output::ZwpDcsOutput`
    // etc. in its generated code.
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

    wayland_scanner::generate_interfaces!("../protocol/zwp_dcs_quadro_sync_v1.xml");
    wayland_scanner::generate_client_code!("../protocol/zwp_dcs_quadro_sync_v1.xml");
}
