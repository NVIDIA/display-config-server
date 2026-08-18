// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! DCS protocol connectivity check for dcs-test.
//!
//! Delegates to `dcs-client` for connection management so the protocol
//! binding code is not duplicated.

/// Summary of DCS Wayland globals detected during the connection roundtrip.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DcsProtocolState {
    /// Whether `zwp_dcs_manager` was advertised in the registry.
    pub manager_found: bool,
    /// Whether `wp_drm_lease_device_v1` was advertised in the registry.
    pub drm_lease_found: bool,
}

/// Connect to DCS and return the global presence state.
///
/// A successful connection means `manager_found = true`.  If the connection
/// fails (DCS not running), returns `Ok` with `manager_found = false` so
/// dcs-test can report a failure rather than propagating an error.
pub fn query_dcs() -> Result<DcsProtocolState, Box<dyn std::error::Error>> {
    match dcs_client::connection::connect() {
        Ok(client) => Ok(DcsProtocolState {
            manager_found: true,
            drm_lease_found: client.drm_lease_found(),
        }),
        Err(_) => Ok(DcsProtocolState {
            manager_found: false,
            drm_lease_found: false,
        }),
    }
}
