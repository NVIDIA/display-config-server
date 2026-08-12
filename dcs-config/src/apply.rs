// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Topology apply for DCS clients.

use anyhow::Context;

use crate::connection::DcsClient;
use crate::config::TopologyConfig;

impl DcsClient {
    /// Apply a topology to DCS atomically.
    ///
    /// For each display in `topology.display`, finds the matching `wl_output`
    /// by display number (from a previous [`enumerate_outputs`] call or an
    /// implicit one performed here), creates a `zwp_dcs_display_configuration`,
    /// sets the requested mode, and adds it to a `zwp_dcs_topology`.  Commits
    /// the topology and waits for success or an error event.
    pub fn apply(&mut self, topology: &TopologyConfig) -> anyhow::Result<()> {
        // Ensure we have bound_outputs for display-number lookup.
        if self.state.bound_outputs.is_empty() {
            self.enumerate_outputs()?;
        }

        // Clone what we need to avoid simultaneous borrows of `self`.
        let manager = self.manager.clone();
        let bound_outputs = self.state.bound_outputs.clone();

        let wl_topology = manager.create_topology(&self.qh, ());

        for display in &topology.display {
            let bound = bound_outputs
                .iter()
                .find(|b| b.info.display_number == display.number as i32)
                .with_context(|| {
                    format!(
                        "display {} not found — run `dcs-tool show` to list available displays",
                        display.number
                    )
                })?;

            // get_output with () user data: events are ignored (we already
            // have info from enumerate_outputs).
            let dcs_out = manager.get_output(&bound.wl_output, &self.qh, ());
            let cfg = dcs_out.create_configuration(&self.qh, ());

            if let Some(mode) = &display.mode {
                cfg.set_mode(mode.width, mode.height, mode.refresh_mhz);
            }

            wl_topology.add_configuration(&cfg);
        }

        // Reset error flags before commit.
        self.state.topology_error = false;
        self.state.config_error = false;

        wl_topology.commit();

        self.event_queue
            .roundtrip(&mut self.state)
            .context("roundtrip failed after topology commit")?;

        if self.state.topology_error || self.state.config_error {
            anyhow::bail!(
                "topology commit rejected by DCS — verify display numbers and mode values"
            );
        }

        Ok(())
    }
}
