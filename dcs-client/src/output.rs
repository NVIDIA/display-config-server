// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Output enumeration for DCS clients.

use anyhow::Context;

use crate::config::QuadroSyncRole;
use crate::connection::{BoundOutput, DcsClient, PendingOutput};

/// One display mode reported by the server.
#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (e.g. 60000 for 60 Hz).
    pub refresh_mhz: u32,
    /// True if this is the currently active mode.
    pub current: bool,
    /// True if this is the preferred (native) mode.
    pub preferred: bool,
}

/// QuadroSync state of one display, as reported by the server.
#[derive(Debug, Clone)]
pub struct QuadroSyncOutputInfo {
    /// Current framelock role.
    pub role: QuadroSyncRole,
    /// True if framelock sync is currently active on this display.
    pub sync_active: bool,
    /// QuadroSync board this display's GPU is attached to, if reported.
    pub board: Option<u32>,
}

/// Current state of one DCS-managed display, as reported by the server.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// 1-based display index (from the `number` event).
    pub display_number: i32,
    /// DRM device number (dev_t truncated to u32).
    pub dev_t: u32,
    /// All modes reported by the server for this output.
    pub modes: Vec<ModeInfo>,
    /// QuadroSync state; `None` if QuadroSync hardware is not present.
    pub quadro_sync: Option<QuadroSyncOutputInfo>,
}

impl OutputInfo {
    /// Returns the currently active mode, if any.
    pub fn current_mode(&self) -> Option<&ModeInfo> {
        self.modes.iter().find(|m| m.current)
    }
}

impl DcsClient {
    /// Enumerate all displays currently managed by DCS.
    ///
    /// For each `wl_output` global collected during [`connect`], calls
    /// `zwp_dcs_manager.get_output`, does a roundtrip to collect all
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
        let qs_manager = self.state.quadro_sync_manager.clone();
        for (i, wl_output) in wl_outputs.iter().enumerate() {
            let dcs_out = manager.get_output(wl_output, &self.qh, i);
            if let Some(qs) = &qs_manager {
                qs.get_output(&dcs_out, &self.qh, i);
            }
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
                dev_t: pending.dev_t.unwrap_or(0),
                modes: pending.modes.clone(),
                quadro_sync: pending.qs_role.map(|raw| QuadroSyncOutputInfo {
                    // role enum: 0 = disabled, 1 = server, 2 = client.
                    role: match raw {
                        1 => QuadroSyncRole::Server,
                        2 => QuadroSyncRole::Client,
                        _ => QuadroSyncRole::Disabled,
                    },
                    sync_active: pending.qs_sync_active.unwrap_or(false),
                    board: pending.qs_board,
                }),
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
