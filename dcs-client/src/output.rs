// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Output enumeration for DCS clients.

use anyhow::Context;

use crate::config::QuadroSyncRole;
use crate::connection::{BoundOutput, DcsClient, PendingOutput};

/// One display mode reported by the server.
#[derive(Debug, Clone)]
pub struct ModeInfo {
    /// Opaque id the server uses for this mode on this output. Passed back
    /// in `set_mode`; has no meaning beyond identity.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (e.g. 60000 for 60 Hz).
    pub refresh_mhz: u32,
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
    /// DRM device number (st_rdev / dev_t of the DRM primary node), decoded
    /// from the `device` event. `0` if the server did not report one.
    pub dev_t: u64,
    /// All modes reported by the server for this output.
    pub modes: Vec<ModeInfo>,
    /// Id of the active mode (from `current_mode`), if the server sent one.
    pub current_mode_id: Option<u32>,
    /// Id of the preferred mode (from `preferred_mode`), if the server sent one.
    pub preferred_mode_id: Option<u32>,
    /// QuadroSync state; `None` if QuadroSync hardware is not present.
    pub quadro_sync: Option<QuadroSyncOutputInfo>,
}

impl OutputInfo {
    /// Returns the currently active mode, if any.
    pub fn current_mode(&self) -> Option<&ModeInfo> {
        self.mode_by_id(self.current_mode_id?)
    }

    /// Returns the preferred (native) mode, if any.
    pub fn preferred_mode(&self) -> Option<&ModeInfo> {
        self.mode_by_id(self.preferred_mode_id?)
    }

    /// Returns the advertised mode with this server-assigned id.
    pub fn mode_by_id(&self, id: u32) -> Option<&ModeInfo> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// Returns the advertised mode matching a (width, height, refresh mHz)
    /// triple, if this display supports it.
    pub fn find_mode(&self, width: u32, height: u32, refresh_mhz: u32) -> Option<&ModeInfo> {
        self.modes
            .iter()
            .find(|m| m.width == width && m.height == height && m.refresh_mhz == refresh_mhz)
    }
}

/// Decode the `zwp_dcs_output.device` payload.
///
/// The server sends `sizeof(dev_t)` bytes in native byte order, the same
/// encoding `wp_linux_dmabuf_feedback.main_device` uses. Any width up to 64
/// bits is accepted so the client does not bake in the server's dev_t size.
/// Returns `None` for an empty or oversized payload.
pub(crate) fn dev_t_from_bytes(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > std::mem::size_of::<u64>() {
        return None;
    }
    let mut buf = [0u8; 8];
    if cfg!(target_endian = "little") {
        buf[..bytes.len()].copy_from_slice(bytes);
    } else {
        buf[8 - bytes.len()..].copy_from_slice(bytes);
    }
    Some(u64::from_ne_bytes(buf))
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
        let mut dcs_outputs = Vec::with_capacity(wl_outputs.len());
        let mut qs_outputs = Vec::new();
        for (i, wl_output) in wl_outputs.iter().enumerate() {
            let dcs_out = manager.get_output(wl_output, &self.qh, i);
            if let Some(qs) = &qs_manager {
                qs_outputs.push(qs.get_output(&dcs_out, &self.qh, i));
            }
            dcs_outputs.push(dcs_out);
        }

        // Roundtrip: server sends mode/device/number/done for each output.
        let roundtrip = self
            .event_queue
            .roundtrip(&mut self.state)
            .context("roundtrip failed while enumerating outputs");

        // The query objects have served their purpose once the events are
        // in; the wl_output proxies are what `apply` needs later.
        for qs_out in qs_outputs {
            qs_out.destroy();
        }
        for dcs_out in dcs_outputs {
            dcs_out.destroy();
        }

        roundtrip?;

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
                current_mode_id: pending.current_mode_id,
                preferred_mode_id: pending.preferred_mode_id,
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

#[cfg(test)]
mod tests {
    use super::dev_t_from_bytes;

    #[test]
    fn dev_t_round_trips_native_u64() {
        let value: u64 = 0x0000_0000_e200_0000;
        assert_eq!(dev_t_from_bytes(&value.to_ne_bytes()), Some(value));
    }

    #[test]
    fn dev_t_accepts_narrower_encodings() {
        let value: u32 = 0xe200;
        assert_eq!(dev_t_from_bytes(&value.to_ne_bytes()), Some(value as u64));
    }

    #[test]
    fn dev_t_rejects_empty_and_oversized() {
        assert_eq!(dev_t_from_bytes(&[]), None);
        assert_eq!(dev_t_from_bytes(&[0u8; 9]), None);
    }
}
