// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! QuadroSync (framelock) attribute implementations and nvidia-drm ioctl bindings.

use std::os::unix::io::RawFd;

use anyhow::{anyhow, Context};

use crate::render::dcs_output::OutputHandle;
use crate::render::dcs_output::DcsOutput;
use crate::render::quadro_sync::{board_for_device, QuadroSyncBoard};
use crate::render::DcsDevice;
use crate::DcsState;
use super::{DisplayAttribute, TopologyAttribute};

// ---------------------------------------------------------------------------
// nvidia-drm ioctl constants and structs
// ---------------------------------------------------------------------------

// DRM_COMMAND_BASE is 0x40 on Linux and FreeBSD for nvidia-drm
const DRM_COMMAND_BASE: u32 = 0x40;

const DRM_NVIDIA_GET_DEV_INFO: u32 = 0x03;
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

#[repr(C)]
struct DrmNvidiaGetDevInfoParams {
    gpu_id: u32,
    mig_device: u32,
    primary_index: u32,
    supports_alloc: u32,
    generic_page_kind: u32,
    page_kind_generation: u32,
    sector_layout: u32,
    supports_sync_fd: u32,
    supports_semsurf: u32,
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

/// The nvidia-drm GPU id of the device behind `drm_fd`. This is the same id
/// space as `QuadroSyncState::gpu_ids`, so it tells which board entries refer
/// to this device.
pub fn query_gpu_id(drm_fd: RawFd) -> anyhow::Result<u32> {
    let mut params = DrmNvidiaGetDevInfoParams {
        gpu_id: 0,
        mig_device: 0,
        primary_index: 0,
        supports_alloc: 0,
        generic_page_kind: 0,
        page_kind_generation: 0,
        sector_layout: 0,
        supports_sync_fd: 0,
        supports_semsurf: 0,
    };
    nvidia_drm_ioctl(
        drm_fd,
        drm_iowr::<DrmNvidiaGetDevInfoParams>(DRM_NVIDIA_GET_DEV_INFO),
        &mut params,
    )
    .context("DRM_IOCTL_NVIDIA_GET_DEV_INFO")?;
    Ok(params.gpu_id)
}

/// Persistent QuadroSync hardware state, populated at DcsDevice startup.
pub struct QuadroSyncState {
    pub num_boards: u32,
    /// This device's own GPU id (from GET_DEV_INFO).
    pub gpu_id: u32,
    /// GPU ids bound to the QuadroSync board attached to this device.
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

    let gpu_id = match query_gpu_id(drm_fd) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                "RTX PRO Sync: board detected but GPU id query failed ({}); QuadroSync disabled on this device",
                e
            );
            return None;
        }
    };

    Some(QuadroSyncState {
        num_boards: params.num_framelocks,
        gpu_id,
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
    /// Per-output role assignments (output handle, role).
    pub roles: Vec<(OutputHandle, QuadroSyncRoleAttribute)>,
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

    fn display_attributes(&self) -> &[(OutputHandle, Box<dyn DisplayAttribute>)] {
        // Roles are stored as concrete types and applied in apply(); nothing to
        // return here as standalone DisplayAttributes.
        &[]
    }

    fn output_handles(&self) -> Vec<OutputHandle> {
        self.roles.iter().map(|(handle, _)| *handle).collect()
    }

    fn validate(&self, state: &DcsState) -> anyhow::Result<()> {
        let mut servers = 0u32;
        let mut clients = 0u32;

        for (handle, role_attr) in &self.roles {
            let output = state
                .output_for_handle(*handle)
                .ok_or_else(|| anyhow!("QuadroSync role references unknown output {:?}", handle))?;
            if state.board_for_device(handle.device_index).is_none() {
                return Err(anyhow!(
                    "display {} is on a GPU without QuadroSync hardware and cannot take a QuadroSync role",
                    output.display_number
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

    fn apply(&self, state: &mut DcsState) -> anyhow::Result<()> {
        let mut io = DrmQuadroSyncIoctls::new(&state.devices, self.framelock_index);
        self.apply_with(&state.quadro_sync_boards, &mut io)
    }
}

impl QuadroSyncTopologyAttribute {
    /// The global apply sequence, over any `QuadroSyncIoctls`. Each connector
    /// ioctl goes to the device owning that connector; board attributes go to
    /// the server's board only; sync is enabled on the server before clients;
    /// engagement is checked once per board involved.
    ///
    /// If a step fails, sync stays disabled on every role connector (step 1
    /// already ran) and any roles already set remain in place. Re-applying a
    /// valid topology is the remedy.
    pub fn apply_with(&self, boards: &[QuadroSyncBoard], io: &mut dyn QuadroSyncIoctls) -> anyhow::Result<()> {
        // Step 1: Disable sync on every role connector first (best-effort).
        // NVKMS rejects role changes while sync is enabled.
        for (handle, role_attr) in &self.roles {
            let _ = io.set_display_sync(handle.device_index, role_attr.connector_id, false);
        }

        // Step 2: Set roles, each on its owning device.
        for (handle, role_attr) in &self.roles {
            tracing::info!(
                "RTX PRO Sync: configuring connector {} on device {} as QuadroSync {}",
                role_attr.connector_id,
                handle.device_index,
                role_attr.role.describe()
            );
            io.set_display_config(handle.device_index, role_attr.connector_id, role_attr.role.to_drm_config())
                .context("failed to set QuadroSync display config")?;
        }

        // Step 3: Board attributes on the server's board only. House sync,
        // polarity, and delay are properties of the sync source; a client
        // board locks to the incoming chain signal.
        let server = self
            .roles
            .iter()
            .find(|(_, r)| r.role == QuadroSyncRole::Server)
            .map(|(handle, _)| *handle);
        match server {
            Some(server_handle) => {
                let dev = server_handle.device_index;
                if let Some(delay) = self.sync_delay {
                    io.set_attribute(dev, NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_DELAY, delay as i64)
                        .context("failed to set QuadroSync sync delay")?;
                }
                if let Some(polarity) = self.polarity {
                    io.set_attribute(dev, NV_KMS_FRAMELOCK_ATTRIBUTE_POLARITY, polarity as i64)
                        .context("failed to set QuadroSync polarity")?;
                }
                if let Some(house_sync) = self.house_sync_mode {
                    io.set_attribute(dev, NV_KMS_FRAMELOCK_ATTRIBUTE_HOUSE_SYNC_MODE, house_sync as i64)
                        .context("failed to set QuadroSync house sync mode")?;
                }
            }
            None if self.sync_delay.is_some() || self.polarity.is_some() || self.house_sync_mode.is_some() => {
                tracing::info!(
                    "RTX PRO Sync: no local server; board attributes not applied because the sync source is on another system"
                );
            }
            None => {}
        }

        let mut checked: Vec<u32> = Vec::new();
        if self.sync_enable {
            // Step 4: Enable sync, server first, then clients.
            let active: Vec<&(OutputHandle, QuadroSyncRoleAttribute)> = self
                .roles
                .iter()
                .filter(|(_, r)| r.role != QuadroSyncRole::Disabled)
                .collect();
            tracing::info!(
                "RTX PRO Sync: enabling sync on {} display(s), this system is a QuadroSync {}",
                active.len(),
                if server.is_some() { "server" } else { "client, expecting sync signal from the chain" }
            );
            for (handle, role_attr) in active.iter().filter(|(_, r)| r.role == QuadroSyncRole::Server) {
                io.set_display_sync(handle.device_index, role_attr.connector_id, true)
                    .context("failed to enable QuadroSync sync on server")?;
            }
            for (handle, role_attr) in active.iter().filter(|(_, r)| r.role == QuadroSyncRole::Client) {
                io.set_display_sync(handle.device_index, role_attr.connector_id, true)
                    .context("failed to enable QuadroSync sync on client")?;
            }

            // Step 5: One engagement check per board involved, on the first
            // device we have for that board.
            for (handle, _) in &active {
                let Some(board) = board_for_device(boards, handle.device_index) else { continue };
                if !checked.contains(&board) {
                    checked.push(board);
                    io.check_engagement(handle.device_index);
                }
            }
        } else {
            tracing::info!("RTX PRO Sync: sync disabled");
        }

        tracing::info!(
            "QuadroSync configuration applied: {} roles across {} board(s), sync_enable={}",
            self.roles.len(),
            checked.len(),
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

// ---------------------------------------------------------------------------
// ioctl routing
// ---------------------------------------------------------------------------

/// The QuadroSync ioctls the topology apply needs, addressed by device index so
/// each call lands on the fd of the GPU that owns the connector or board.
/// Implemented for real DRM fds and for a recording fake in tests.
pub trait QuadroSyncIoctls {
    fn set_display_sync(&mut self, device_index: usize, connector_id: u32, enable: bool) -> anyhow::Result<()>;
    fn set_display_config(&mut self, device_index: usize, connector_id: u32, config: u32) -> anyhow::Result<()>;
    fn set_attribute(&mut self, device_index: usize, attribute: u32, value: i64) -> anyhow::Result<()>;
    /// Log current engagement and arm the delayed re-check on this device's board.
    fn check_engagement(&mut self, device_index: usize);
}

/// Real implementation over the DRM fds of `DcsState::devices`.
pub struct DrmQuadroSyncIoctls<'a> {
    devices: &'a [DcsDevice],
    framelock_index: u32,
}

impl<'a> DrmQuadroSyncIoctls<'a> {
    pub fn new(devices: &'a [DcsDevice], framelock_index: u32) -> Self {
        Self { devices, framelock_index }
    }

    fn fd(&self, device_index: usize) -> anyhow::Result<RawFd> {
        use std::os::unix::io::{AsFd, AsRawFd};
        self.devices
            .get(device_index)
            .map(|d| d.drm_device.as_fd().as_raw_fd())
            .ok_or_else(|| anyhow!("no device at index {}", device_index))
    }
}

impl QuadroSyncIoctls for DrmQuadroSyncIoctls<'_> {
    fn set_display_sync(&mut self, device_index: usize, connector_id: u32, enable: bool) -> anyhow::Result<()> {
        let mut params = DrmNvidiaFramelockSetDisplaySyncParams { connector_id, enable: enable as u32 };
        nvidia_drm_ioctl(
            self.fd(device_index)?,
            drm_iow::<DrmNvidiaFramelockSetDisplaySyncParams>(DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_SYNC),
            &mut params,
        )
        .with_context(|| format!("SET_DISPLAY_SYNC connector {} on device {}", connector_id, device_index))
    }

    fn set_display_config(&mut self, device_index: usize, connector_id: u32, config: u32) -> anyhow::Result<()> {
        let mut params = DrmNvidiaFramelockSetDisplayConfigParams { connector_id, config };
        nvidia_drm_ioctl(
            self.fd(device_index)?,
            drm_iow::<DrmNvidiaFramelockSetDisplayConfigParams>(DRM_NVIDIA_FRAMELOCK_SET_DISPLAY_CONFIG),
            &mut params,
        )
        .with_context(|| format!("SET_DISPLAY_CONFIG connector {} on device {}", connector_id, device_index))
    }

    fn set_attribute(&mut self, device_index: usize, attribute: u32, value: i64) -> anyhow::Result<()> {
        let mut params = DrmNvidiaFramelockSetAttributeParams {
            framelock_index: self.framelock_index,
            attribute,
            value,
        };
        nvidia_drm_ioctl(
            self.fd(device_index)?,
            drm_iow::<DrmNvidiaFramelockSetAttributeParams>(DRM_NVIDIA_FRAMELOCK_SET_ATTRIBUTE),
            &mut params,
        )
        .with_context(|| format!("SET_ATTRIBUTE {} on device {}", attribute, device_index))
    }

    fn check_engagement(&mut self, device_index: usize) {
        let Ok(fd) = self.fd(device_index) else { return };
        match get_sync_ready(fd, self.framelock_index) {
            Ok(true) => tracing::info!("RTX PRO Sync: sync engaged on device {}", device_index),
            Ok(false) => tracing::info!(
                "RTX PRO Sync: sync not yet engaged on device {}, re-checking shortly",
                device_index
            ),
            Err(e) => tracing::warn!("RTX PRO Sync: sync ready query failed on device {}: {}", device_index, e),
        }
        spawn_engagement_check(fd, self.framelock_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::quadro_sync::QuadroSyncBoard;
    use crate::render::dcs_output::OutputHandle;
    use std::num::NonZeroU32;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Sync { device: usize, connector: u32, enable: bool },
        Config { device: usize, connector: u32, config: u32 },
        Attr { device: usize, attribute: u32, value: i64 },
        Engage { device: usize },
    }

    #[derive(Default)]
    struct RecordingIoctls {
        calls: Vec<Call>,
    }

    impl QuadroSyncIoctls for RecordingIoctls {
        fn set_display_sync(&mut self, device_index: usize, connector_id: u32, enable: bool) -> anyhow::Result<()> {
            self.calls.push(Call::Sync { device: device_index, connector: connector_id, enable });
            Ok(())
        }
        fn set_display_config(&mut self, device_index: usize, connector_id: u32, config: u32) -> anyhow::Result<()> {
            self.calls.push(Call::Config { device: device_index, connector: connector_id, config });
            Ok(())
        }
        fn set_attribute(&mut self, device_index: usize, attribute: u32, value: i64) -> anyhow::Result<()> {
            self.calls.push(Call::Attr { device: device_index, attribute, value });
            Ok(())
        }
        fn check_engagement(&mut self, device_index: usize) {
            self.calls.push(Call::Engage { device: device_index });
        }
    }

    fn handle(device_index: usize, raw_crtc: u32) -> OutputHandle {
        OutputHandle::new(device_index, drm::control::crtc::Handle::from(NonZeroU32::new(raw_crtc).unwrap()))
    }

    fn role(device_index: usize, connector_id: u32, role: QuadroSyncRole) -> (OutputHandle, QuadroSyncRoleAttribute) {
        (handle(device_index, connector_id), QuadroSyncRoleAttribute { connector_id, role })
    }

    fn two_boards() -> Vec<QuadroSyncBoard> {
        vec![
            QuadroSyncBoard { id: 0, device_indices: vec![0] },
            QuadroSyncBoard { id: 1, device_indices: vec![1] },
        ]
    }

    fn one_bridging_board() -> Vec<QuadroSyncBoard> {
        vec![QuadroSyncBoard { id: 0, device_indices: vec![0, 1] }]
    }

    fn attr(roles: Vec<(OutputHandle, QuadroSyncRoleAttribute)>, sync_enable: bool) -> QuadroSyncTopologyAttribute {
        QuadroSyncTopologyAttribute {
            roles,
            sync_delay: Some(3),
            polarity: None,
            house_sync_mode: None,
            sync_enable,
            framelock_index: 0,
        }
    }

    fn positions(calls: &[Call], pred: impl Fn(&Call) -> bool) -> Vec<usize> {
        calls.iter().enumerate().filter(|(_, c)| pred(c)).map(|(i, _)| i).collect()
    }

    #[test]
    fn disables_precede_all_role_sets_and_go_to_owning_device() {
        let a = attr(vec![role(0, 10, QuadroSyncRole::Server), role(1, 20, QuadroSyncRole::Client)], true);
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();

        let disables = positions(&io.calls, |c| matches!(c, Call::Sync { enable: false, .. }));
        let configs = positions(&io.calls, |c| matches!(c, Call::Config { .. }));
        assert_eq!(disables.len(), 2);
        assert!(disables.iter().max() < configs.iter().min(), "{:?}", io.calls);
        assert!(io.calls.contains(&Call::Sync { device: 0, connector: 10, enable: false }));
        assert!(io.calls.contains(&Call::Sync { device: 1, connector: 20, enable: false }));
        assert!(io.calls.contains(&Call::Config { device: 1, connector: 20, config: FRAMELOCK_DISPLAY_CONFIG_CLIENT }));
    }

    #[test]
    fn board_attributes_go_only_to_servers_board() {
        // Server on device 1 (board 1); client on device 0 (board 0).
        let a = attr(vec![role(0, 10, QuadroSyncRole::Client), role(1, 20, QuadroSyncRole::Server)], true);
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();
        let attrs: Vec<&Call> = io.calls.iter().filter(|c| matches!(c, Call::Attr { .. })).collect();
        assert_eq!(attrs, vec![&Call::Attr { device: 1, attribute: NV_KMS_FRAMELOCK_ATTRIBUTE_SYNC_DELAY, value: 3 }]);
    }

    #[test]
    fn no_local_server_means_no_board_attributes() {
        let a = attr(vec![role(0, 10, QuadroSyncRole::Client), role(1, 20, QuadroSyncRole::Client)], true);
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();
        assert!(!io.calls.iter().any(|c| matches!(c, Call::Attr { .. })));
    }

    #[test]
    fn server_enable_precedes_every_client_enable() {
        let a = attr(
            vec![role(0, 10, QuadroSyncRole::Client), role(1, 20, QuadroSyncRole::Server), role(1, 21, QuadroSyncRole::Client)],
            true,
        );
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();
        let server_on = positions(&io.calls, |c| matches!(c, Call::Sync { connector: 20, enable: true, .. }));
        let client_on = positions(&io.calls, |c| matches!(c, Call::Sync { enable: true, connector, .. } if *connector != 20));
        assert_eq!(server_on.len(), 1);
        assert_eq!(client_on.len(), 2);
        assert!(server_on[0] < *client_on.iter().min().unwrap(), "{:?}", io.calls);
    }

    #[test]
    fn one_engagement_check_per_board() {
        // Two devices on one bridging board: exactly one check.
        let a = attr(vec![role(0, 10, QuadroSyncRole::Server), role(1, 20, QuadroSyncRole::Client)], true);
        let mut io = RecordingIoctls::default();
        a.apply_with(&one_bridging_board(), &mut io).unwrap();
        assert_eq!(io.calls.iter().filter(|c| matches!(c, Call::Engage { .. })).count(), 1);

        // Two devices on two boards: two checks.
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();
        assert_eq!(io.calls.iter().filter(|c| matches!(c, Call::Engage { .. })).count(), 2);
    }

    #[test]
    fn sync_disabled_skips_enable_and_engagement() {
        let a = attr(vec![role(0, 10, QuadroSyncRole::Server)], false);
        let mut io = RecordingIoctls::default();
        a.apply_with(&two_boards(), &mut io).unwrap();
        assert!(!io.calls.iter().any(|c| matches!(c, Call::Sync { enable: true, .. } | Call::Engage { .. })));
    }

    #[test]
    fn two_servers_across_devices_rejected() {
        assert!(validate_role_counts(2, 0, true).is_err());
    }

    #[test]
    fn get_dev_info_params_matches_kernel_layout() {
        // struct drm_nvidia_get_dev_info_params: nine uint32_t fields.
        assert_eq!(std::mem::size_of::<DrmNvidiaGetDevInfoParams>(), 9 * 4);
    }

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
