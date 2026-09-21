// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::OwnedFd;

use anyhow::Context;

use drm::control::{connector, crtc, Device as ControlDevice};
use drm_fourcc::DrmFormat;
use smithay::{
    backend::{
        drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmNode},
        egl::{EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    },
    utils::DeviceFd,
    wayland::drm_lease::DrmLeaseState,
};

use crate::attribute::quadro_sync::{detect_quadro_sync, QuadroSyncState};

use super::dcs_output::DcsOutput;

/// The 32×32 RGBA PNG splash icon, embedded at compile time.
const ICON_PNG: &[u8] = include_bytes!("../../assets/plus_icon.png");

/// All rendering resources belonging to a single DRM device (GPU).
///
/// One `DcsDevice` is created per GPU. It owns the `GlesRenderer` that is
/// shared across every output on that GPU, and a map of [`DcsOutput`]
/// instances keyed by `crtc::Handle` for O(1) VBlank dispatch.
pub struct DcsDevice {
    /// DRM device — kept public so the DRM lease handler can build leases.
    pub drm_device: DrmDevice,
    /// DRM node for this device, used to create and look up the lease global.
    pub drm_node: DrmNode,
    /// Shared renderer for all outputs on this GPU.
    renderer: GlesRenderer,
    /// Active outputs keyed by the CRTC they drive.
    pub outputs: HashMap<crtc::Handle, DcsOutput>,
    /// wp_drm_lease_device_v1 protocol state for this device.
    pub drm_lease_state: Option<DrmLeaseState>,
    /// Populated at startup if QuadroSync hardware is detected.
    pub quadro_sync_state: Option<QuadroSyncState>,
}

/// Decode the embedded PNG icon and return raw RGBA8 bytes, row-major.
fn decode_icon() -> anyhow::Result<Vec<u8>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    anyhow::ensure!(
        info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight,
        "icon PNG must be 8-bit RGBA (got {:?} / {:?})",
        info.color_type,
        info.bit_depth
    );
    buf.truncate(info.buffer_size());
    Ok(buf)
}

impl DcsDevice {
    /// Open `drm_path`, initialise the shared GBM/EGL/GLES rendering stack,
    /// and create a [`DcsOutput`] for every connected connector on the device.
    ///
    /// Returns the device alongside the `DrmDeviceNotifier` that the caller
    /// must register with calloop to receive VBlank events.
    ///
    /// Display numbers are assigned from `first_display_number` upward so
    /// that numbers stay unique across devices.
    /// `filter_connector` limits DCS to the connector with that DRM object id,
    /// for debugging. `None` initialises every connected connector.
    pub fn new(
        drm_path: &str,
        first_display_number: i32,
        filter_connector: Option<u32>,
    ) -> anyhow::Result<(Self, DrmDeviceNotifier)> {
        let drm_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(drm_path)?;
        let drm_fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(drm_file)));
        let gbm_fd = drm_fd.clone();

        let (mut drm_device, drm_notifier) = DrmDevice::new(drm_fd, false)?;

        // ------------------------------------------------------------------ //
        // GBM → EGL → GLES renderer (shared across all outputs on this GPU)
        // ------------------------------------------------------------------ //
        let gbm_device = gbm::Device::new(gbm_fd)?;
        // SAFETY: no other thread holds the GBM device at this point.
        let egl_display = unsafe { EGLDisplay::new(gbm_device.clone()) }?;
        let egl_context = EGLContext::new(&egl_display)?;
        // SAFETY: the EGLContext is not current in any other thread.
        let renderer = unsafe { GlesRenderer::new(egl_context) }?;

        let render_formats: Vec<DrmFormat> = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect();

        // ------------------------------------------------------------------ //
        // Enumerate connected connectors; create one DcsOutput per CRTC
        // ------------------------------------------------------------------ //
        let icon_rgba = decode_icon()?;
        let dev_t = std::fs::metadata(drm_path)?.rdev();
        let resources = drm_device.resource_handles()?;
        let mut outputs: HashMap<crtc::Handle, DcsOutput> = HashMap::new();
        let mut used_crtcs: HashSet<crtc::Handle> = HashSet::new();
        let mut display_number: i32 = first_display_number;

        if let Some(id) = filter_connector {
            tracing::info!("--connector {}: only initialising that connector", id);
        }

        for &connector_handle in resources.connectors() {
            let raw_id: u32 = connector_handle.into();
            if let Some(id) = filter_connector {
                tracing::info!("--connector filter: connector raw_id={} vs filter={}", raw_id, id);
                if raw_id != id {
                    continue;
                }
            }

            let connector_info = match drm_device.get_connector(connector_handle, false) {
                Ok(info) => info,
                Err(e) => {
                    tracing::warn!("Failed to query connector {:?}: {}", connector_handle, e);
                    continue;
                }
            };

            if connector_info.state() != connector::State::Connected {
                continue;
            }

            match DcsOutput::new(
                &mut drm_device,
                gbm_device.clone(),
                &render_formats,
                connector_info,
                &icon_rgba,
                &used_crtcs,
                display_number,
                dev_t,
            ) {
                Ok((crtc, output)) => {
                    used_crtcs.insert(crtc);
                    outputs.insert(crtc, output);
                    display_number += 1;
                }
                Err(e) => tracing::warn!("Failed to create output: {:#}", e),
            }
        }

        anyhow::ensure!(
            !outputs.is_empty(),
            "No connected outputs found on {}",
            drm_path
        );

        tracing::info!("{} output(s) initialised on {}", outputs.len(), drm_path);

        let drm_node = DrmNode::from_file(drm_device.device_fd())
            .context("failed to get DrmNode from device fd")?;

        // Detect QuadroSync hardware via nvidia-drm ioctl.
        let quadro_sync_state = {
            use std::os::unix::io::{AsFd, AsRawFd};
            let raw_fd = drm_device.as_fd().as_raw_fd();
            let qs = detect_quadro_sync(raw_fd);
            if let Some(ref s) = qs {
                tracing::info!("QuadroSync detected: {} board(s)", s.num_boards);
            } else {
                tracing::info!("No QuadroSync hardware detected");
            }
            qs
        };

        Ok((
            DcsDevice {
                drm_device,
                drm_node,
                renderer,
                outputs,
                drm_lease_state: None,
                quadro_sync_state,
            },
            drm_notifier,
        ))
    }

    /// Render all outputs that have `needs_render` set, skipping idle ones.
    pub fn render(&mut self) -> anyhow::Result<()> {
        for output in self.outputs.values_mut() {
            output.render_with(&mut self.renderer)?;
        }
        Ok(())
    }

    /// Resolve `(width, height, refresh_mhz)` to the matching `drm::control::Mode`
    /// reported by the connector attached to `crtc`.
    ///
    /// Shared by [`validate_output_mode_change`] and [`commit_output_mode_change`].
    fn resolve_mode_for_crtc(
        &self,
        crtc: crtc::Handle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<drm::control::Mode> {
        let connector_handle = self
            .outputs
            .get(&crtc)
            .context("no output for CRTC")?
            .current_connectors()
            .into_iter()
            .next()
            .context("output has no connectors")?;

        let connector_info = self
            .drm_device
            .get_connector(connector_handle, false)
            .context("failed to query connector")?;

        connector_info
            .modes()
            .iter()
            .find(|m: &&drm::control::Mode| {
                let (mw, mh) = m.size();
                mw as u32 == width && mh as u32 == height && m.vrefresh() * 1000 == refresh_mhz
            })
            .copied()
            .with_context(|| {
                format!(
                    "mode {}x{}@{}mHz not found for connector",
                    width, height, refresh_mhz
                )
            })
    }

    /// Test whether the requested mode is accepted by the hardware without
    /// applying it.  Returns an error if the mode is unknown or the atomic
    /// test commit is rejected.
    pub fn validate_output_mode_change(
        &self,
        crtc: crtc::Handle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let mode = self.resolve_mode_for_crtc(crtc, width, height, refresh_mhz)?;
        self.outputs.get(&crtc).unwrap().test_mode_change(mode)
    }

    /// Apply the requested mode to the output driven by `crtc`.
    ///
    /// Stages the change through the `DrmCompositor` (resizing the swapchain)
    /// and marks the output for re-render so the hardware modeset fires on the
    /// next `render_frame` call. No test commit is performed here; call
    /// [`validate_output_mode_change`] first if validation is required.
    pub fn commit_output_mode_change(
        &mut self,
        crtc: crtc::Handle,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let mode = self.resolve_mode_for_crtc(crtc, width, height, refresh_mhz)?;
        self.outputs.get_mut(&crtc).unwrap().apply_mode_change(mode)
    }

    /// Notify the output driving `crtc` that its queued frame has been
    /// scanned out.  Called on every `DrmEvent::VBlank(crtc)`.
    pub fn frame_submitted(&mut self, crtc: crtc::Handle) -> anyhow::Result<()> {
        if let Some(output) = self.outputs.get_mut(&crtc) {
            output.frame_submitted()?;
        }
        Ok(())
    }
}
