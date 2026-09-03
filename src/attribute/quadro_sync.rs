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
const DRM_NVIDIA_FRAMELOCK_GET_ATTRIBUTE: u32 = 0x21;
const DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_CONFIG: u32 = 0x23;
const DRM_NVIDIA_FRAMELOCK_GET_DISPLAY_CONFIG: u32 = 0x24;
const DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_SYNC: u32 = 0x25;
const DRM_NVIDIA_FRAMELOCK_GET_DISPLAY_SYNC: u32 = 0x26;

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

#[repr(C)]
struct DrmNvidiaFramelockGetAttributeParams {
    framelock_index: u32,
    attribute: u32,
    value: i64,
    __pad: u32,
}

#[repr(C)]
struct DrmNvidiaFramelockGetDisplayConfigParams {
    connector_id: u32,
    config: u32,
}

#[repr(C)]
struct DrmNvidiaFramelockGetDisplaySyncParams {
    connector_id: u32,
    enabled: u32,
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

    fn from_drm_config(value: u32) -> Self {
        match value {
            FRAMELOCK_DISPLAY_CONFIG_SERVER => QuadroSyncRole::Server,
            FRAMELOCK_DISPLAY_CONFIG_CLIENT => QuadroSyncRole::Client,
            _ => QuadroSyncRole::Disabled,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            QuadroSyncRole::Disabled => "disabled",
            QuadroSyncRole::Server => "server",
            QuadroSyncRole::Client => "client",
        }
    }
}

// ---------------------------------------------------------------------------
// Status queries
// ---------------------------------------------------------------------------

/// Query whether display sync is enabled on a connector.
pub fn get_display_sync(fd: RawFd, connector_id: u32) -> anyhow::Result<bool> {
    let mut params = DrmNvidiaFramelockGetDisplaySyncParams {
        connector_id,
        enabled: 0,
    };
    nvidia_drm_ioctl(
        fd,
        drm_iowr::<DrmNvidiaFramelockGetDisplaySyncParams>(DRM_NVIDIA_FRAMELOCK_GET_DISPLAY_SYNC),
        &mut params,
    )
    .context("failed to get framelock display sync")?;
    Ok(params.enabled != 0)
}

/// Query the current framelock role configured on a connector.
pub fn get_display_config(fd: RawFd, connector_id: u32) -> anyhow::Result<QuadroSyncRole> {
    let mut params = DrmNvidiaFramelockGetDisplayConfigParams {
        connector_id,
        config: 0,
    };
    nvidia_drm_ioctl(
        fd,
        drm_iowr::<DrmNvidiaFramelockGetDisplayConfigParams>(
            DRM_NVIDIA_FRAMELOCK_GET_DISPLAY_CONFIG,
        ),
        &mut params,
    )
    .context("failed to get framelock display config")?;
    Ok(QuadroSyncRole::from_drm_config(params.config))
}

/// Query whether the framelock board reports sync ready (locked to the sync
/// signal, either its own as the server or the incoming one as a client).
pub fn get_sync_ready(fd: RawFd, framelock_index: u32) -> anyhow::Result<bool> {
    let mut params = DrmNvidiaFramelockGetAttributeParams {
        framelock_index,
        attribute: NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_READY,
        value: 0,
        __pad: 0,
    };
    nvidia_drm_ioctl(
        fd,
        drm_iowr::<DrmNvidiaFramelockGetAttributeParams>(DRM_NVIDIA_FRAMELOCK_GET_ATTRIBUTE),
        &mut params,
    )
    .context("failed to get framelock sync ready")?;
    Ok(params.value != 0)
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

// NvKmsFrameLockAttribute enum values for the SET/GET_ATTRIBUTE ioctls.
// These must match enum NvKmsFrameLockAttribute in nvkms-api-types.h, which
// the drm_nvidia_framelock_*_attribute_params structs pass through verbatim.
const NV_KMS_FRAMELOCK_ATTRIBUTE_POLARITY: u32 = 0;
const NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_DELAY: u32 = 1;
const NV_KMS_FRAMELOCK_ATTRIBUTE_HOUSE_SYNC_MODE: u32 = 2;
const NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_READY: u32 = 4;

/// Validate the local role counts for a sync-enable request.
///
/// Server-only and client-only topologies are valid: in a multi-system
/// framelock chain the counterpart displays live on other machines connected
/// via the RJ45 ports, which this machine cannot see. The operator is
/// responsible for the chain having exactly one server overall. Locally we
/// only require that at least one display has a role (there is nothing to
/// sync otherwise) and that at most one display is the server.
fn validate_role_counts(servers: u32, clients: u32, sync_enable: bool) -> anyhow::Result<()> {
    if !sync_enable {
        return Ok(());
    }

    if servers > 1 {
        return Err(anyhow!(
            "QuadroSync allows at most one server per system, found {}",
            servers
        ));
    }
    if servers + clients == 0 {
        return Err(anyhow!(
            "QuadroSync sync enable requires at least one display with a server or client role"
        ));
    }

    Ok(())
}

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

        validate_role_counts(servers, clients, self.sync_enable)
    }

    fn apply(&self, device: &mut DcsDevice) -> anyhow::Result<()> {
        use std::os::unix::io::{AsFd, AsRawFd};

        let fd = device.drm_device.as_fd().as_raw_fd();

        // Step 1: Disable sync before changing roles (best-effort). NVKMS
        // rejects role changes while sync is enabled. The connector_id must
        // be a real connector for the DRM lookup to succeed, so issue the
        // disable per role-bearing connector.
        for (_crtc, role_attr) in &self.roles {
            let mut params = DrmNvidiaFramelockSetDisplaySyncParams {
                connector_id: role_attr.connector_id,
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
            tracing::info!(
                "RTX PRO Sync: configuring connector {} as framelock {}",
                role_attr.connector_id,
                role_attr.role.describe()
            );
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

        // Step 4: Enable sync if requested. Sync is enabled on every
        // connector with a role, server and clients alike, matching the
        // nvidia-settings model. On a client-only system (multi-system
        // framelock chain) the server lives on another machine.
        if self.sync_enable {
            let role_connectors: Vec<(u32, QuadroSyncRole)> = self
                .roles
                .iter()
                .filter(|(_, r)| r.role != QuadroSyncRole::Disabled)
                .map(|(_, r)| (r.connector_id, r.role))
                .collect();

            let has_server = role_connectors
                .iter()
                .any(|(_, role)| *role == QuadroSyncRole::Server);

            tracing::info!(
                "RTX PRO Sync: enabling sync on {} display(s), this system is a framelock {}",
                role_connectors.len(),
                if has_server {
                    "server"
                } else {
                    "client, expecting sync signal from the chain"
                }
            );

            for (connector_id, _role) in &role_connectors {
                let mut params = DrmNvidiaFramelockSetDisplaySyncParams {
                    connector_id: *connector_id,
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

            // Log the initial engagement state. Locking can take a moment,
            // especially for a client waiting on the incoming signal, so also
            // arm a one-shot delayed re-check for the operator.
            match get_sync_ready(fd, self.framelock_index) {
                Ok(true) => tracing::info!("RTX PRO Sync: sync engaged"),
                Ok(false) => {
                    tracing::info!("RTX PRO Sync: sync not yet engaged, re-checking shortly")
                }
                Err(e) => tracing::warn!("RTX PRO Sync: sync ready query failed: {}", e),
            }

            spawn_engagement_check(fd, self.framelock_index);
        } else {
            tracing::info!("RTX PRO Sync: sync disabled");
        }

        tracing::info!(
            "QuadroSync configuration applied: {} roles, sync_enable={}",
            self.roles.len(),
            self.sync_enable
        );

        Ok(())
    }
}

/// One-shot delayed re-check of framelock engagement, so the operator gets a
/// definitive "engaged" or "not engaged" log line after the board has had
/// time to lock. Runs on a detached thread with a dup'd fd so it does not
/// touch any server state.
fn spawn_engagement_check(fd: RawFd, framelock_index: u32) {
    const ENGAGEMENT_CHECK_DELAY_SECS: u64 = 30;

    let check_fd = unsafe { libc::dup(fd) };
    if check_fd < 0 {
        tracing::warn!("RTX PRO Sync: could not dup DRM fd for engagement check");
        return;
    }

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(ENGAGEMENT_CHECK_DELAY_SECS));
        match get_sync_ready(check_fd, framelock_index) {
            Ok(true) => tracing::info!("RTX PRO Sync: sync engaged and in use"),
            Ok(false) => tracing::warn!(
                "RTX PRO Sync: sync not engaged after {}s, check chain cabling and \
                 that exactly one system in the chain is the framelock server",
                ENGAGEMENT_CHECK_DELAY_SECS
            ),
            Err(e) => tracing::warn!("RTX PRO Sync: engagement check failed: {}", e),
        }
        unsafe { libc::close(check_fd) };
    });
}

#[cfg(test)]
mod tests {
    use super::validate_role_counts;

    #[test]
    fn sync_disabled_allows_anything() {
        assert!(validate_role_counts(0, 0, false).is_ok());
        assert!(validate_role_counts(2, 0, false).is_ok());
    }

    #[test]
    fn server_and_client_ok() {
        assert!(validate_role_counts(1, 1, true).is_ok());
        assert!(validate_role_counts(1, 3, true).is_ok());
    }

    #[test]
    fn server_only_ok_for_multi_system() {
        assert!(validate_role_counts(1, 0, true).is_ok());
    }

    #[test]
    fn client_only_ok_for_multi_system() {
        assert!(validate_role_counts(0, 1, true).is_ok());
        assert!(validate_role_counts(0, 2, true).is_ok());
    }

    #[test]
    fn two_servers_rejected() {
        assert!(validate_role_counts(2, 0, true).is_err());
        assert!(validate_role_counts(2, 1, true).is_err());
    }

    #[test]
    fn no_roles_rejected() {
        assert!(validate_role_counts(0, 0, true).is_err());
    }
}
