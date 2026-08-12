// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
use crate::connection::DcsClient;

/// Current state of one DCS-managed display.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub display_number: i32,
    pub mode_width: u32,
    pub mode_height: u32,
    pub mode_refresh_mhz: u32,
    pub dev_t: u32,
}

impl DcsClient {
    /// Enumerate all displays currently managed by DCS.
    ///
    /// Calls `zwp_dcs_manager.get_output` for each `wl_output` global,
    /// collects `mode`, `device`, `number`, and `done` events, and returns
    /// one [`OutputInfo`] per display.
    pub fn enumerate_outputs(&mut self) -> anyhow::Result<Vec<OutputInfo>> {
        todo!("implemented in Task 4")
    }
}
