// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Server-side request handlers for the `zwp_dcs_quadro_sync_v1` sub-protocol.

use std::sync::Mutex;

use drm::control::crtc;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::attribute::quadro_sync::{
    HouseSyncMode, QuadroSyncPolarity, QuadroSyncRole, QuadroSyncRoleAttribute,
    QuadroSyncTopologyAttribute,
};
use crate::protocols::zwp_dcs_quadro_sync_v1::{
    zwp_dcs_quadro_sync_display_configuration::{
        self, ZwpDcsQuadroSyncDisplayConfiguration,
    },
    zwp_dcs_quadro_sync_manager::{self, ZwpDcsQuadroSyncManager},
    zwp_dcs_quadro_sync_output::{self, Role as OutputRole, ZwpDcsQuadroSyncOutput},
    zwp_dcs_quadro_sync_topology::{
        self, HouseSyncMode as ProtoHouseSyncMode, Polarity as ProtoPolarity,
        ZwpDcsQuadroSyncTopology,
    },
};
use wayland_server::WEnum;
use crate::protocol::zwp_display_config_server_v1::WlDcsOutput;
use crate::DcsState;

// ---------------------------------------------------------------------------
// User data types
// ---------------------------------------------------------------------------

/// User data for a `zwp_dcs_quadro_sync_output` resource.
pub struct WlQuadroSyncOutput {
    /// CRTC of the base DCS output this extends.
    pub crtc: Option<crtc::Handle>,
}

/// User data for a `zwp_dcs_quadro_sync_display_configuration` resource.
pub struct WlQuadroSyncDisplayConfiguration {
    /// Inherited from the base DCS output.
    pub crtc: Option<crtc::Handle>,
    /// Raw DRM connector id, resolved from the base DCS output at bind time.
    pub connector_id: Option<u32>,
    /// Role staged by `set_role`.
    pub pending_role: Option<QuadroSyncRole>,
}

/// User data for a `zwp_dcs_quadro_sync_topology` resource.
pub struct WlQuadroSyncTopology {
    pub pending_sync_delay: Option<u32>,
    pub pending_polarity: Option<QuadroSyncPolarity>,
    pub pending_house_sync_mode: Option<HouseSyncMode>,
    pub pending_sync_enable: Option<bool>,
    /// QuadroSync display configurations associated with this topology.
    pub configurations: Vec<ZwpDcsQuadroSyncDisplayConfiguration>,
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
                // Look up the CRTC from the base DCS output's user data.
                let crtc = output.data::<WlDcsOutput>().and_then(|d| d.crtc);

                let resource = data_init.init(id, WlQuadroSyncOutput { crtc });

                // Send initial QuadroSync state for this output.
                // Hardware query is deferred to future work; send placeholders.
                if let Some(crtc_handle) = crtc {
                    if state.output_for_crtc(crtc_handle).is_some() {
                        resource.sync_status(0); // not currently synced
                        resource.role(OutputRole::Disabled);
                        resource.done();
                    }
                }
            }
            zwp_dcs_quadro_sync_manager::Request::GetTopology { id, topology } => {
                let qs_topo = data_init.init(
                    id,
                    Mutex::new(WlQuadroSyncTopology {
                        pending_sync_delay: None,
                        pending_polarity: None,
                        pending_house_sync_mode: None,
                        pending_sync_enable: None,
                        configurations: Vec::new(),
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
        }
    }
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
        data: &WlQuadroSyncOutput,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_dcs_quadro_sync_output::Request::GetConfiguration { id, config: _ } => {
                let crtc = data.crtc;

                // connector_id is resolved during commit via the CRTC handle;
                // nothing to look up here without DcsState access.
                data_init.init(
                    id,
                    Mutex::new(WlQuadroSyncDisplayConfiguration {
                        crtc,
                        connector_id: None,
                        pending_role: None,
                    }),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Display Configuration: Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<ZwpDcsQuadroSyncDisplayConfiguration, Mutex<WlQuadroSyncDisplayConfiguration>>
    for DcsState
{
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpDcsQuadroSyncDisplayConfiguration,
        request: zwp_dcs_quadro_sync_display_configuration::Request,
        data: &Mutex<WlQuadroSyncDisplayConfiguration>,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut config = data.lock().unwrap();
        match request {
            zwp_dcs_quadro_sync_display_configuration::Request::SetRole { role } => {
                // role is WEnum<zwp_dcs_quadro_sync_output::Role>; extract numeric value.
                use crate::protocols::zwp_dcs_quadro_sync_v1::zwp_dcs_quadro_sync_output::Role as ProtoRole;
                config.pending_role = match role {
                    WEnum::Value(ProtoRole::Disabled) => Some(QuadroSyncRole::Disabled),
                    WEnum::Value(ProtoRole::Server) => Some(QuadroSyncRole::Server),
                    WEnum::Value(ProtoRole::Client) => Some(QuadroSyncRole::Client),
                    _ => None,
                };
            }
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
        }
    }
}

// ---------------------------------------------------------------------------
// DcsState: build QuadroSync attributes from topology state
// ---------------------------------------------------------------------------

impl DcsState {
    /// Build a [`QuadroSyncTopologyAttribute`] from the QuadroSync topology
    /// resource associated with the given base topology, if any, and push it
    /// into `pending_commit`.
    ///
    /// Called by the base topology's `Commit` handler before `apply_topology`.
    pub fn build_quadro_sync_attribute(
        &mut self,
        qs_topo_resource: &ZwpDcsQuadroSyncTopology,
    ) -> anyhow::Result<()> {
        use crate::attribute::PendingCommit;

        let topo = qs_topo_resource
            .data::<Mutex<WlQuadroSyncTopology>>()
            .ok_or_else(|| anyhow::anyhow!("QuadroSync topology has no user data"))?
            .lock()
            .unwrap();

        let mut roles = Vec::new();

        for qs_config_resource in &topo.configurations {
            let qs_config = qs_config_resource
                .data::<Mutex<WlQuadroSyncDisplayConfiguration>>()
                .ok_or_else(|| {
                    anyhow::anyhow!("QuadroSync display config has no user data")
                })?
                .lock()
                .unwrap();

            let Some(crtc) = qs_config.crtc else {
                continue;
            };

            let role = qs_config
                .pending_role
                .unwrap_or(QuadroSyncRole::Disabled);

            // Resolve the connector_id from the DcsOutput for this CRTC.
            let connector_id = self
                .output_for_crtc(crtc)
                .map(|o| {
                    let raw: u32 = o.connector_handle.into();
                    raw
                })
                .unwrap_or(0);

            roles.push((crtc, QuadroSyncRoleAttribute { connector_id, role }));
        }

        let attr = QuadroSyncTopologyAttribute {
            roles,
            sync_delay: topo.pending_sync_delay,
            polarity: topo.pending_polarity,
            house_sync_mode: topo.pending_house_sync_mode,
            sync_enable: topo.pending_sync_enable.unwrap_or(false),
            framelock_index: 0,
        };

        let commit = self
            .pending_commit
            .get_or_insert_with(PendingCommit::new);
        commit.topology_attrs.push(Box::new(attr));

        Ok(())
    }
}
