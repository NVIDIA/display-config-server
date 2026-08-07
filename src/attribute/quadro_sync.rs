// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! QuadroSync (framelock) attribute implementations and nvidia-drm ioctl bindings.

use std::os::unix::io::RawFd;

use anyhow::{anyhow, Context};
use drm::control::crtc;

use crate::render::dcs_output::DcsOutput;
use crate::render::DcsDevice;
use super::{DisplayAttribute, TopologyAttribute};

// ---------------------------------------------------------------------------
// nvidia-drm ioctl constants and structs
// ---------------------------------------------------------------------------

// DRM_COMMAND_BASE is 0x40 on Linux and FreeBSD for nvidia-drm
const DRM_COMMAND_BASE: u32 = 0x40;

const DRM_NVIDIA_FRAMELOCK_QUERY: u32 = 0x1f;
const DRM_NVIDIA_FRAMELOCK_SET_ATTRIBUTE: u32 = 0x20;
const DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_CONFIG: u32 = 0x23;
const DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_SYNC: u32 = 0x25;

const FRAMELOCK_DISPLAY_CONFIG_DISABLED: u32 = 0;
const FRAMELOCK_DISPLAY_CONFIG_SERVER: u32 = 1;
const FRAMELOCK_DISPLAY_CONFIG_CLIENT: u32 = 2;

#[repr(C)]
struct DrmNvidiaFramelockQueryParams {
    num_framelocks: u32,
    max_gpus_per_framelock: u32,
    gpu_ids: [u32; 16], // 4 boards * 4 GPUs
    __pad: u32,
}

#[repr(C)]
struct DrmNvidiaFramelockSetAttributeParams {
    framelock_index: u32,
    attribute: u32,
    value: i64,
}

#[repr(C)]
struct DrmNvidiaFramelockSetDisplayConfigParams {
    connector_id: u32,
    config: u32,
}

#[repr(C)]
struct DrmNvidiaFramelockSetDisplaySyncParams {
    connector_id: u32,
    enable: u32,
}

/// Build a DRM IOWR ioctl request number.
/// _IOWR('d', DRM_COMMAND_BASE + nr, T)
fn drm_iowr<T>(nr: u32) -> libc::c_ulong {
    let size = std::mem::size_of::<T>() as libc::c_ulong;
    (3 << 30) | (size << 16) | (0x64 << 8) | ((DRM_COMMAND_BASE + nr) as libc::c_ulong)
}

/// Build a DRM IOW ioctl request number.
/// _IOW('d', DRM_COMMAND_BASE + nr, T)
fn drm_iow<T>(nr: u32) -> libc::c_ulong {
    let size = std::mem::size_of::<T>() as libc::c_ulong;
    (1 << 30) | (size << 16) | (0x64 << 8) | ((DRM_COMMAND_BASE + nr) as libc::c_ulong)
}

fn nvidia_drm_ioctl<T>(fd: RawFd, request: libc::c_ulong, arg: &mut T) -> anyhow::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request, arg as *mut T) };
    if ret < 0 {
        Err(anyhow!(
            "nvidia-drm ioctl failed: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hardware detection
// ---------------------------------------------------------------------------

/// Persistent QuadroSync hardware state, populated at DcsDevice startup.
pub struct QuadroSyncState {
    pub num_boards: u32,
    pub gpu_ids: Vec<u32>,
}

/// Query the DRM device for QuadroSync (framelock) hardware.
/// Returns `Some(QuadroSyncState)` if boards are found, `None` otherwise.
pub fn detect_quadro_sync(drm_fd: RawFd) -> Option<QuadroSyncState> {
    let mut params = DrmNvidiaFramelockQueryParams {
        num_framelocks: 0,
        max_gpus_per_framelock: 0,
        gpu_ids: [0u32; 16],
        __pad: 0,
    };

    let request = drm_iowr::<DrmNvidiaFramelockQueryParams>(DRM_NVIDIA_FRAMELOCK_QUERY);
    if nvidia_drm_ioctl(drm_fd, request, &mut params).is_err() {
        return None;
    }

    if params.num_framelocks == 0 {
        return None;
    }

    let count = (params.num_framelocks as usize)
        .saturating_mul(params.max_gpus_per_framelock as usize)
        .min(16);
    let gpu_ids = params.gpu_ids[..count]
        .iter()
        .copied()
        .filter(|&id| id != 0)
        .collect();

    Some(QuadroSyncState {
        num_boards: params.num_framelocks,
        gpu_ids,
    })
}

// ---------------------------------------------------------------------------
// QuadroSync role enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuadroSyncRole {
    Disabled,
    Server,
    Client,
}

impl QuadroSyncRole {
    fn to_drm_config(self) -> u32 {
        match self {
            QuadroSyncRole::Disabled => FRAMELOCK_DISPLAY_CONFIG_DISABLED,
            QuadroSyncRole::Server => FRAMELOCK_DISPLAY_CONFIG_SERVER,
            QuadroSyncRole::Client => FRAMELOCK_DISPLAY_CONFIG_CLIENT,
        }
    }

    pub fn from_protocol(value: u32) -> anyhow::Result<Self> {
        match value {
            0 => Ok(QuadroSyncRole::Disabled),
            1 => Ok(QuadroSyncRole::Server),
            2 => Ok(QuadroSyncRole::Client),
            _ => Err(anyhow!("invalid QuadroSync role: {}", value)),
        }
    }
}

// ---------------------------------------------------------------------------
// QuadroSync polarity and house sync enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum QuadroSyncPolarity {
    RisingEdge = 0x1,
    FallingEdge = 0x2,
    BothEdges = 0x3,
}

#[derive(Debug, Clone, Copy)]
pub enum HouseSyncMode {
    Disabled = 0,
    Input = 1,
    Output = 2,
}

// ---------------------------------------------------------------------------
// Per-output: QuadroSyncRoleAttribute
// ---------------------------------------------------------------------------

/// Per-output framelock role assignment.
pub struct QuadroSyncRoleAttribute {
    pub connector_id: u32,
    pub role: QuadroSyncRole,
}

impl DisplayAttribute for QuadroSyncRoleAttribute {
    fn name(&self) -> &str {
        "quadro_sync_role"
    }

    fn validate(&self, _output: &DcsOutput) -> anyhow::Result<()> {
        // Role validation is done at the topology level (check one server, etc.)
        Ok(())
    }

    fn apply(&self, _output: &mut DcsOutput) -> anyhow::Result<()> {
        // Per-output roles are applied by the parent QuadroSyncTopologyAttribute,
        // which has access to the DRM fd. This no-op is only here to satisfy
        // the DisplayAttribute trait when used standalone.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cross-output: QuadroSyncTopologyAttribute
// ---------------------------------------------------------------------------

// NvKmsFrameLockAttribute enum values for SET_ATTRIBUTE ioctl
const NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_DELAY: u32 = 0;
const NV_KMS_FRAMELOCK_ATTRIBUTE_HOUSE_SYNC_MODE: u32 = 3;
const NV_KMS_FRAMELOCK_ATTRIBUTE_POLARITY: u32 = 6;

/// Cross-output framelock board configuration.  Owns per-output role attributes.
///
/// Roles are stored directly (not as `Box<dyn DisplayAttribute>`) so we can
/// access `connector_id` and `role` without downcasting.  `display_attributes()`
/// returns an empty slice; the parent applies its children itself.
pub struct QuadroSyncTopologyAttribute {
    /// Per-output role assignments (crtc, role).
    pub roles: Vec<(crtc::Handle, QuadroSyncRoleAttribute)>,
    /// Board-level settings.
    pub sync_delay: Option<u32>,
    pub polarity: Option<QuadroSyncPolarity>,
    pub house_sync_mode: Option<HouseSyncMode>,
    pub sync_enable: bool,
    /// Framelock board index (0 for the first board).
    pub framelock_index: u32,
}

impl TopologyAttribute for QuadroSyncTopologyAttribute {
    fn name(&self) -> &str {
        "quadro_sync"
    }

    fn display_attributes(&self) -> &[(crtc::Handle, Box<dyn DisplayAttribute>)] {
        // Roles are stored as concrete types and applied in apply(); nothing to
        // return here as standalone DisplayAttributes.
        &[]
    }

    fn validate(&self, device: &DcsDevice) -> anyhow::Result<()> {
        let mut servers = 0u32;
        let mut clients = 0u32;

        for (crtc, role_attr) in &self.roles {
            if !device.outputs.contains_key(crtc) {
                return Err(anyhow!(
                    "QuadroSync role references unknown CRTC {:?}",
                    crtc
                ));
            }
            match role_attr.role {
                QuadroSyncRole::Server => servers += 1,
                QuadroSyncRole::Client => clients += 1,
                QuadroSyncRole::Disabled => {}
            }
        }

        if self.sync_enable {
            if servers != 1 {
                return Err(anyhow!(
                    "QuadroSync requires exactly one server, found {}",
                    servers
                ));
            }
            if clients == 0 {
                return Err(anyhow!("QuadroSync requires at least one client"));
            }
        }

        Ok(())
    }

    fn apply(&self, device: &mut DcsDevice) -> anyhow::Result<()> {
        use std::os::unix::io::{AsFd, AsRawFd};

        let fd = device.drm_device.as_fd().as_raw_fd();

        // Step 1: Disable sync before changing roles (best-effort)
        {
            let mut params = DrmNvidiaFramelockSetDisplaySyncParams {
                connector_id: 0,
                enable: 0,
            };
            let _ = nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetDisplaySyncParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_SYNC,
                ),
                &mut params,
            );
        }

        // Step 2: Set per-output roles
        for (_crtc, role_attr) in &self.roles {
            let mut params = DrmNvidiaFramelockSetDisplayConfigParams {
                connector_id: role_attr.connector_id,
                config: role_attr.role.to_drm_config(),
            };
            nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetDisplayConfigParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_CONFIG,
                ),
                &mut params,
            )
            .context("failed to set framelock display config")?;
        }

        // Step 3: Set board attributes
        if let Some(delay) = self.sync_delay {
            let mut params = DrmNvidiaFramelockSetAttributeParams {
                framelock_index: self.framelock_index,
                attribute: NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_DELAY,
                value: delay as i64,
            };
            nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetAttributeParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_ATTRIBUTE,
                ),
                &mut params,
            )
            .context("failed to set framelock sync delay")?;
        }

        if let Some(polarity) = self.polarity {
            let mut params = DrmNvidiaFramelockSetAttributeParams {
                framelock_index: self.framelock_index,
                attribute: NV_KMS_FRAMELOCK_ATTRIBUTE_POLARITY,
                value: polarity as i64,
            };
            nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetAttributeParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_ATTRIBUTE,
                ),
                &mut params,
            )
            .context("failed to set framelock polarity")?;
        }

        if let Some(house_sync) = self.house_sync_mode {
            let mut params = DrmNvidiaFramelockSetAttributeParams {
                framelock_index: self.framelock_index,
                attribute: NV_KMS_FRAMELOCK_ATTRIBUTE_HOUSE_SYNC_MODE,
                value: house_sync as i64,
            };
            nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetAttributeParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_ATTRIBUTE,
                ),
                &mut params,
            )
            .context("failed to set framelock house sync mode")?;
        }

        // Step 4: Enable sync if requested
        if self.sync_enable {
            let server_connector = self
                .roles
                .iter()
                .find(|(_, r)| r.role == QuadroSyncRole::Server)
                .map(|(_, r)| r.connector_id)
                .ok_or_else(|| anyhow!("no server connector for sync enable"))?;

            let mut params = DrmNvidiaFramelockSetDisplaySyncParams {
                connector_id: server_connector,
                enable: 1,
            };
            nvidia_drm_ioctl(
                fd,
                drm_iow::<DrmNvidiaFramelockSetDisplaySyncParams>(
                    DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_SYNC,
                ),
                &mut params,
            )
            .context("failed to enable framelock sync")?;
        }

        tracing::info!(
            "QuadroSync configuration applied: {} roles, sync_enable={}",
            self.roles.len(),
            self.sync_enable
        );

        Ok(())
    }
}
