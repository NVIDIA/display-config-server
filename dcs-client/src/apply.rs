// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Topology apply for DCS clients.

use anyhow::Context;

use crate::connection::DcsClient;
use crate::config::{HouseSyncMode, QuadroSyncPolarity, QuadroSyncRole, TopologyConfig};
use crate::protocol::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_output::Role as ProtoRole;
use crate::protocol::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_topology::{
    HouseSyncMode as ProtoHouseSyncMode, Polarity as ProtoPolarity,
};

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

        let wants_quadro_sync = topology.quadro_sync.is_some()
            || topology.display.iter().any(|d| d.quadro_sync_role.is_some());
        let qs_manager = self.state.quadro_sync_manager.clone();
        if wants_quadro_sync && qs_manager.is_none() {
            anyhow::bail!(
                "configuration contains QuadroSync settings but DCS did not detect \
                 QuadroSync hardware"
            );
        }

        let wl_topology = manager.create_topology(&self.qh, ());

        let mut qs_topology = None;
        if wants_quadro_sync {
            if let Some(qs_manager) = &qs_manager {
                qs_topology = Some(qs_manager.get_topology(&wl_topology, &self.qh, ()));
            }
        }

        if let (Some(qs), Some(qs_topology)) = (&topology.quadro_sync, &qs_topology) {
            if let Some(delay) = qs.sync_delay {
                qs_topology.set_sync_delay(delay);
            }
            if let Some(polarity) = qs.polarity {
                qs_topology.set_polarity(match polarity {
                    QuadroSyncPolarity::RisingEdge => ProtoPolarity::RisingEdge,
                    QuadroSyncPolarity::FallingEdge => ProtoPolarity::FallingEdge,
                    QuadroSyncPolarity::BothEdges => ProtoPolarity::BothEdges,
                });
            }
            if let Some(mode) = qs.house_sync_mode {
                qs_topology.set_house_sync_mode(match mode {
                    HouseSyncMode::Disabled => ProtoHouseSyncMode::Disabled,
                    HouseSyncMode::Input => ProtoHouseSyncMode::Input,
                    HouseSyncMode::Output => ProtoHouseSyncMode::Output,
                });
            }
            if let Some(enable) = qs.sync_enable {
                qs_topology.set_sync_enable(enable as u32);
            }
        }

        // Every protocol object created below is destroyed once the commit
        // result is in, so a long-lived client does not leak server objects.
        let mut created_outputs = Vec::new();
        let mut created_configs = Vec::new();
        let mut created_qs_outputs = Vec::new();
        let mut created_qs_configs = Vec::new();

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

            if let (Some(role), Some(qs_manager)) = (display.quadro_sync_role, &qs_manager) {
                let qs_out = qs_manager.get_output(&dcs_out, &self.qh, ());
                let qs_cfg = qs_out.get_configuration(&cfg, &self.qh, ());
                qs_cfg.set_role(match role {
                    QuadroSyncRole::Disabled => ProtoRole::Disabled,
                    QuadroSyncRole::Server => ProtoRole::Server,
                    QuadroSyncRole::Client => ProtoRole::Client,
                });
                created_qs_outputs.push(qs_out);
                created_qs_configs.push(qs_cfg);
            }

            created_outputs.push(dcs_out);
            created_configs.push(cfg);
        }

        // Reset error flags before commit.
        self.state.topology_error = false;
        self.state.config_error = false;

        wl_topology.commit();

        let roundtrip = self
            .event_queue
            .roundtrip(&mut self.state)
            .context("roundtrip failed after topology commit");

        // Tear down in reverse creation order: extensions first, then the
        // objects they extend, then the topology.
        for qs_cfg in created_qs_configs {
            qs_cfg.destroy();
        }
        for qs_out in created_qs_outputs {
            qs_out.destroy();
        }
        if let Some(qs_topology) = qs_topology {
            qs_topology.destroy();
        }
        for cfg in created_configs {
            cfg.destroy();
        }
        for dcs_out in created_outputs {
            dcs_out.destroy();
        }
        wl_topology.destroy();

        roundtrip?;

        if self.state.topology_error || self.state.config_error {
            anyhow::bail!(
                "topology commit rejected by DCS — verify display numbers and mode values"
            );
        }

        Ok(())
    }
}
