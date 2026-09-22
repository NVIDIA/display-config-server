// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Server-side request handlers for the `zwp_dcs_quadro_sync_v1` sub-protocol.

use std::sync::Mutex;

use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::render::dcs_output::OutputHandle;

use crate::attribute::quadro_sync::{
    HouseSyncMode, QuadroSyncPolarity, QuadroSyncRole, QuadroSyncRoleAttribute,
    QuadroSyncTopologyAttribute,
};
use crate::protocols::zwp_dcs_quadro_sync_v1::{
    zwp_dcs_quadro_sync_manager::{self, ZwpDcsQuadroSyncManager},
    zwp_dcs_quadro_sync_output::{self, Role as OutputRole, ZwpDcsQuadroSyncOutput},
    zwp_dcs_quadro_sync_topology::{
        self, Error as QuadroSyncError, HouseSyncMode as ProtoHouseSyncMode,
        Polarity as ProtoPolarity, ZwpDcsQuadroSyncTopology,
    },
};
use wayland_server::protocol::wl_output::WlOutput;
use wayland_server::WEnum;
use crate::protocol::zwp_display_config_server_v1::WlDcsOutput;
use crate::DcsState;

// ---------------------------------------------------------------------------
// User data types
// ---------------------------------------------------------------------------

/// User data for a `zwp_dcs_quadro_sync_output` resource.
///
/// The object is read-only for the client (its state arrives as events at
/// creation), so nothing needs to be remembered per resource.
pub struct WlQuadroSyncOutput;

/// A framelock role staged for one display by `set_role`.
pub struct PendingRole {
    /// The `wl_output` the client named. Kept so commit errors can point at it.
    pub output: WlOutput,
    /// Resolved from the `wl_output`'s user data; `None` means DCS does not
    /// manage that output, reported as `unknown_output` at commit.
    pub handle: Option<OutputHandle>,
    /// `None` when the client sent a value outside the role enum, reported
    /// as `invalid_role` at commit.
    pub role: Option<QuadroSyncRole>,
}

/// User data for a `zwp_dcs_quadro_sync_topology` resource.
pub struct WlQuadroSyncTopology {
    /// Roles staged by `set_role`, in the order the client first named each
    /// display.
    pub roles: Vec<PendingRole>,
    pub pending_sync_delay: Option<u32>,
    pub pending_polarity: Option<QuadroSyncPolarity>,
    pub pending_house_sync_mode: Option<HouseSyncMode>,
    pub pending_sync_enable: Option<bool>,
}

// ---------------------------------------------------------------------------
// Manager: GlobalDispatch + Dispatch
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZwpDcsQuadroSyncManager, ()> for DcsState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpDcsQuadroSyncManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpDcsQuadroSyncManager, ()> for DcsState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsQuadroSyncManager,
        request: zwp_dcs_quadro_sync_manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_quadro_sync_manager::Request::GetOutput { id, output } => {
                // Look up the OutputHandle from the base DCS output's user data.
                let handle = output.data::<WlDcsOutput>().and_then(|d| d.handle);

                let resource = data_init.init(id, WlQuadroSyncOutput);

                // Send the current QuadroSync state for this output, queried
                // from the hardware.
                if let Some(handle) = handle {
                    if let Some(dcs_output) = state.output_for_handle(handle) {
                        let connector_id: u32 = dcs_output.connector_handle.into();
                        let (role, engaged) = query_output_state(state, handle, connector_id);

                        resource.sync_status(engaged as u32);
                        resource.role(role);
                        if let Some(board) = state.board_for_device(handle.device_index) {
                            resource.board(board);
                        }
                        resource.done();
                    }
                }
            }
            zwp_dcs_quadro_sync_manager::Request::GetTopology { id, topology } => {
                let qs_topo = data_init.init(
                    id,
                    Mutex::new(WlQuadroSyncTopology {
                        roles: Vec::new(),
                        pending_sync_delay: None,
                        pending_polarity: None,
                        pending_house_sync_mode: None,
                        pending_sync_enable: None,
                    }),
                );

                // Register the QuadroSync topology with the base topology so that
                // the commit handler can find and apply QuadroSync attributes.
                use crate::protocol::zwp_display_config_server_v1::WlDcsTopology;
                if let Some(base_data) = topology.data::<Mutex<WlDcsTopology>>() {
                    let mut base = base_data.lock().unwrap();
                    base.quadro_sync_topology = Some(qs_topo);
                }
            }
            zwp_dcs_quadro_sync_manager::Request::Destroy => {}
        }
    }
}

/// Query the hardware for an output's current framelock role and engagement.
///
/// Engagement means display sync is enabled on the connector and the board
/// reports sync ready. Query failures degrade to disabled/not-engaged with a
/// debug log rather than erroring the protocol request.
fn query_output_state(
    state: &DcsState,
    handle: OutputHandle,
    connector_id: u32,
) -> (OutputRole, bool) {
    use std::os::unix::io::{AsFd, AsRawFd};

    use crate::attribute::quadro_sync::{get_display_config, get_display_sync, get_sync_ready};

    let Some(device) = state.device_for_handle(handle) else {
        return (OutputRole::Disabled, false);
    };
    let fd = device.drm_device.as_fd().as_raw_fd();

    let role = match get_display_config(fd, connector_id) {
        Ok(QuadroSyncRole::Server) => OutputRole::Server,
        Ok(QuadroSyncRole::Client) => OutputRole::Client,
        Ok(QuadroSyncRole::Disabled) => OutputRole::Disabled,
        Err(e) => {
            tracing::debug!(
                "framelock display config query failed for connector {}: {}",
                connector_id,
                e
            );
            OutputRole::Disabled
        }
    };

    let enabled = get_display_sync(fd, connector_id).unwrap_or_else(|e| {
        tracing::debug!(
            "framelock display sync query failed for connector {}: {}",
            connector_id,
            e
        );
        false
    });

    let ready = enabled
        && get_sync_ready(fd, 0).unwrap_or_else(|e| {
            tracing::debug!("framelock sync ready query failed: {}", e);
            false
        });

    (role, enabled && ready)
}

// ---------------------------------------------------------------------------
// Output: Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsQuadroSyncOutput, WlQuadroSyncOutput> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsQuadroSyncOutput,
        request: zwp_dcs_quadro_sync_output::Request,
        _data: &WlQuadroSyncOutput,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_quadro_sync_output::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Topology: Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsQuadroSyncTopology, Mutex<WlQuadroSyncTopology>> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsQuadroSyncTopology,
        request: zwp_dcs_quadro_sync_topology::Request,
        data: &Mutex<WlQuadroSyncTopology>,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut topo = data.lock().unwrap();
        match request {
            zwp_dcs_quadro_sync_topology::Request::SetRole { output, role } => {
                let role = match role {
                    WEnum::Value(OutputRole::Disabled) => Some(QuadroSyncRole::Disabled),
                    WEnum::Value(OutputRole::Server) => Some(QuadroSyncRole::Server),
                    WEnum::Value(OutputRole::Client) => Some(QuadroSyncRole::Client),
                    _ => None,
                };
                // One entry per display; a repeated set_role replaces it.
                if let Some(existing) = topo.roles.iter_mut().find(|r| r.output == output) {
                    existing.role = role;
                } else {
                    topo.roles.push(PendingRole {
                        handle: output.data::<OutputHandle>().copied(),
                        output,
                        role,
                    });
                }
            }
            zwp_dcs_quadro_sync_topology::Request::SetSyncDelay { delay } => {
                topo.pending_sync_delay = Some(delay);
            }
            zwp_dcs_quadro_sync_topology::Request::SetPolarity { polarity } => {
                topo.pending_polarity = match polarity {
                    WEnum::Value(ProtoPolarity::RisingEdge) => {
                        Some(QuadroSyncPolarity::RisingEdge)
                    }
                    WEnum::Value(ProtoPolarity::FallingEdge) => {
                        Some(QuadroSyncPolarity::FallingEdge)
                    }
                    WEnum::Value(ProtoPolarity::BothEdges) => {
                        Some(QuadroSyncPolarity::BothEdges)
                    }
                    _ => None,
                };
            }
            zwp_dcs_quadro_sync_topology::Request::SetHouseSyncMode { mode } => {
                topo.pending_house_sync_mode = match mode {
                    WEnum::Value(ProtoHouseSyncMode::Disabled) => {
                        Some(HouseSyncMode::Disabled)
                    }
                    WEnum::Value(ProtoHouseSyncMode::Input) => Some(HouseSyncMode::Input),
                    WEnum::Value(ProtoHouseSyncMode::Output) => Some(HouseSyncMode::Output),
                    _ => None,
                };
            }
            zwp_dcs_quadro_sync_topology::Request::SetSyncEnable { enable } => {
                topo.pending_sync_enable = Some(enable != 0);
            }
            // The base topology prunes a destroyed extension before commit.
            zwp_dcs_quadro_sync_topology::Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// DcsState: build QuadroSync attributes from topology state
// ---------------------------------------------------------------------------

impl DcsState {
    /// Build a [`QuadroSyncTopologyAttribute`] from the state staged on a
    /// QuadroSync topology extension.
    ///
    /// Returns `Ok(None)` when nothing QuadroSync-related was staged, so a
    /// topology that merely created the extension does not touch framelock.
    /// Per-display problems (an output DCS does not manage, an out-of-range
    /// role) are sent as error events on the extension right here, and the
    /// build returns `Err` so the commit is reported as failed. The returned
    /// attribute carries the extension object and each role's `wl_output`
    /// so it can send its own error events during validate() and apply().
    ///
    /// Called by the base topology's `Commit` handler, which owns the
    /// [`PendingCommit`](crate::attribute::PendingCommit) the result goes into.
    pub fn build_quadro_sync_attribute(
        &self,
        qs_topo_resource: &ZwpDcsQuadroSyncTopology,
    ) -> anyhow::Result<Option<QuadroSyncTopologyAttribute>> {
        let topo = qs_topo_resource
            .data::<Mutex<WlQuadroSyncTopology>>()
            .expect("wrong user data type on zwp_dcs_quadro_sync_topology")
            .lock()
            .unwrap();

        let mut roles = Vec::new();
        let mut rejected = 0usize;

        for pending in topo.roles.iter().filter(|r| r.output.is_alive()) {
            let output = pending
                .handle
                .and_then(|handle| self.output_for_handle(handle).map(|o| (handle, o)));
            let Some((handle, dcs_output)) = output else {
                qs_topo_resource.error(Some(&pending.output), QuadroSyncError::UnknownOutput);
                rejected += 1;
                continue;
            };
            let Some(role) = pending.role else {
                qs_topo_resource.error(Some(&pending.output), QuadroSyncError::InvalidRole);
                rejected += 1;
                continue;
            };
            let connector_id: u32 = dcs_output.connector_handle.into();
            roles.push((
                handle,
                QuadroSyncRoleAttribute {
                    connector_id,
                    role,
                    wl_output: Some(pending.output.clone()),
                },
            ));
        }

        if rejected > 0 {
            anyhow::bail!("{} QuadroSync role(s) rejected", rejected);
        }

        let has_board_settings = topo.pending_sync_delay.is_some()
            || topo.pending_polarity.is_some()
            || topo.pending_house_sync_mode.is_some()
            || topo.pending_sync_enable.is_some();
        if roles.is_empty() && !has_board_settings {
            return Ok(None);
        }

        Ok(Some(QuadroSyncTopologyAttribute {
            resource: Some(qs_topo_resource.clone()),
            roles,
            sync_delay: topo.pending_sync_delay,
            polarity: topo.pending_polarity,
            house_sync_mode: topo.pending_house_sync_mode,
            sync_enable: topo.pending_sync_enable.unwrap_or(false),
            framelock_index: 0,
        }))
    }
}
