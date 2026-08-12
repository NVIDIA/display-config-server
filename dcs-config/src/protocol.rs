// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Generated client-side bindings for the DCS private Wayland protocol.
//!
//! Re-exported from `dcs_config::protocol` for use by connection and apply
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
