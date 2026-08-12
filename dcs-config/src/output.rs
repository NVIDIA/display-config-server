// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Output enumeration for DCS clients.

use anyhow::Context;

use crate::connection::{BoundOutput, DcsClient, PendingOutput};

/// Current state of one DCS-managed display, as reported by the server.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// 1-based display index (from the `number` event).
    pub display_number: i32,
    pub mode_width: u32,
    pub mode_height: u32,
    /// Refresh rate in millihertz (e.g. 60000 for 60 Hz).
    pub mode_refresh_mhz: u32,
    /// DRM device number (dev_t truncated to u32).
    pub dev_t: u32,
}

impl DcsClient {
    /// Enumerate all displays currently managed by DCS.
    ///
    /// For each `wl_output` global collected during [`connect`], calls
    /// `zwp_dcs_manager.get_output`, does a roundtrip to collect
    /// `mode`/`device`/`number`/`done` events, and returns one
    /// [`OutputInfo`] per display.
    ///
    /// Also caches the results internally so that [`apply`] can look up
    /// `wl_output` proxies by display number without a second roundtrip.
    pub fn enumerate_outputs(&mut self) -> anyhow::Result<Vec<OutputInfo>> {
        let manager = self.manager.clone();
        let wl_outputs = self.state.wl_outputs.clone();

        if wl_outputs.is_empty() {
            return Ok(Vec::new());
        }

        // Initialise one pending slot per wl_output.
        self.state.pending = vec![PendingOutput::default(); wl_outputs.len()];

        // Request DCS output info for each wl_output.  User data = index so
        // the Dispatch<ZwpDcsOutput, usize> impl knows which slot to fill.
        for (i, wl_output) in wl_outputs.iter().enumerate() {
            manager.get_output(wl_output, &self.qh, i);
        }

        // Roundtrip: server sends mode/device/number/done for each output.
        self.event_queue
            .roundtrip(&mut self.state)
            .context("roundtrip failed while enumerating outputs")?;

        // Build BoundOutput list (internal) and OutputInfo list (public).
        let mut bound = Vec::new();
        let mut infos = Vec::new();

        for (wl_output, pending) in wl_outputs.iter().zip(self.state.pending.iter()) {
            if !pending.done {
                continue;
            }
            let info = OutputInfo {
                display_number: pending.display_number.unwrap_or(-1),
                mode_width: pending.mode_width,
                mode_height: pending.mode_height,
                mode_refresh_mhz: pending.mode_refresh_mhz,
                dev_t: pending.dev_t.unwrap_or(0),
            };
            bound.push(BoundOutput {
                wl_output: wl_output.clone(),
                info: info.clone(),
            });
            infos.push(info);
        }

        self.state.bound_outputs = bound;
        Ok(infos)
    }
}
