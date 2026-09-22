// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Topology apply for DCS clients.

use anyhow::Context;
use wayland_client::WEnum;
use wayland_client::protocol::wl_output::WlOutput;

use crate::connection::{BoundOutput, DcsClient};
use crate::config::{HouseSyncMode, QuadroSyncPolarity, QuadroSyncRole, TopologyConfig};
use crate::protocol::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_output::Role as ProtoRole;
use crate::protocol::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_topology::{
    Error as QuadroSyncError, HouseSyncMode as ProtoHouseSyncMode, Polarity as ProtoPolarity,
};
use crate::protocol::zwp_display_config_server_v1::zwp_dcs_topology::Error as TopologyError;

impl DcsClient {
    /// Apply a topology to DCS atomically.
    ///
    /// For each display in `topology.display`, finds the matching `wl_output`
    /// by display number (from a previous [`enumerate_outputs`] call or an
    /// implicit one performed here), stages its mode and QuadroSync role on
    /// a `zwp_dcs_topology` (and its QuadroSync extension), then commits and
    /// waits for the reply. On failure the error names every display the
    /// server rejected.
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

        // Every per-display request names the wl_output directly; there are
        // no per-display protocol objects to create or tear down.
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

            if let Some(mode) = &display.mode {
                // The config names modes by geometry; the protocol wants the
                // id the server advertised for that geometry on this display.
                let advertised = bound
                    .info
                    .find_mode(mode.width, mode.height, mode.refresh_mhz)
                    .with_context(|| {
                        format!(
                            "display {} does not support {}x{}@{}mHz — run `dcs-tool show` \
                             to list its modes",
                            display.number, mode.width, mode.height, mode.refresh_mhz
                        )
                    })?;
                wl_topology.set_mode(&bound.wl_output, advertised.id);
            }

            if let (Some(role), Some(qs_topology)) = (display.quadro_sync_role, &qs_topology) {
                qs_topology.set_role(
                    &bound.wl_output,
                    match role {
                        QuadroSyncRole::Disabled => ProtoRole::Disabled,
                        QuadroSyncRole::Server => ProtoRole::Server,
                        QuadroSyncRole::Client => ProtoRole::Client,
                    },
                );
            }
        }

        // Reset the reply state before commit.
        self.state.topology_done = false;
        self.state.topology_success = false;
        self.state.topology_errors.clear();
        self.state.quadro_sync_errors.clear();

        wl_topology.commit();

        let roundtrip = self
            .event_queue
            .roundtrip(&mut self.state)
            .context("roundtrip failed after topology commit");

        // The protocol requires the topology (and its extension) to be
        // destroyed after done; on error the tool has nothing to retry with,
        // so it tears down in that case too.
        if let Some(qs_topology) = qs_topology {
            qs_topology.destroy();
        }
        wl_topology.destroy();

        roundtrip?;

        // The server ends every commit reply with done, and the roundtrip
        // above returns only after it has processed the commit, so a missing
        // done is a server bug rather than a slow server.
        if !self.state.topology_done {
            anyhow::bail!("DCS did not acknowledge the topology commit");
        }

        if !self.state.topology_success {
            anyhow::bail!(
                "topology commit rejected by DCS:{}",
                describe_failures(
                    &bound_outputs,
                    &self.state.topology_errors,
                    &self.state.quadro_sync_errors,
                )
            );
        }

        Ok(())
    }
}

/// One line per display-specific error, with the display named by its DCS
/// number. The terminating topology-wide `failed` carries no extra
/// information when display lines exist, so it is only mentioned when it is
/// all the server sent.
fn describe_failures(
    bound_outputs: &[BoundOutput],
    topology_errors: &[(WlOutput, WEnum<TopologyError>)],
    quadro_sync_errors: &[(Option<WlOutput>, WEnum<QuadroSyncError>)],
) -> String {
    let display_name = |output: &WlOutput| -> String {
        bound_outputs
            .iter()
            .find(|b| b.wl_output == *output)
            .map(|b| format!("display {}", b.info.display_number))
            .unwrap_or_else(|| String::from("unknown display"))
    };

    let mut lines = Vec::new();

    for (output, error) in topology_errors {
        let reason = match error {
            WEnum::Value(TopologyError::UnknownOutput) => "not a display managed by DCS",
            WEnum::Value(TopologyError::InvalidMode) => {
                "the requested mode is not advertised by this display"
            }
            WEnum::Value(TopologyError::InvalidState) => {
                "the requested state could not be validated or applied"
            }
            WEnum::Unknown(code) => {
                lines.push(format!("\n  unknown topology error code {}", code));
                continue;
            }
        };
        lines.push(format!("\n  {}: {}", display_name(output), reason));
    }

    for (output, error) in quadro_sync_errors {
        let reason = match error {
            WEnum::Value(QuadroSyncError::UnknownOutput) => "not a display managed by DCS",
            WEnum::Value(QuadroSyncError::InvalidRole) => "invalid QuadroSync role",
            WEnum::Value(QuadroSyncError::NoHardware) => {
                "this display's GPU has no QuadroSync hardware"
            }
            WEnum::Value(QuadroSyncError::RefreshMismatch) => {
                "refresh rate differs from the rest of the framelock group"
            }
            WEnum::Value(QuadroSyncError::Failed) => {
                "QuadroSync settings rejected as a whole (check server/client roles and sync)"
            }
            WEnum::Unknown(code) => {
                lines.push(format!("\n  unknown QuadroSync error code {}", code));
                continue;
            }
        };
        match output {
            Some(output) => {
                lines.push(format!("\n  {} (QuadroSync): {}", display_name(output), reason))
            }
            None => lines.push(format!("\n  QuadroSync: {}", reason)),
        }
    }

    if lines.is_empty() {
        String::from(" no display-specific detail was reported (see the DCS log)")
    } else {
        lines.concat()
    }
}
