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
