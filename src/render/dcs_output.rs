// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashSet;

use anyhow::Context;
use drm::control::{connector, crtc, Device as ControlDevice};
use drm_fourcc::{DrmFormat, DrmFourcc};
use smithay::wayland::drm_lease::DrmLease;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags},
            Fourcc,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd, PlaneClaim, PlaneState,
        },
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Kind,
            },
            gles::GlesRenderer,
        },
    },
    output::OutputModeSource,
    utils::{Scale, Size, Transform},
};

/// Grid cell size in pixels. The icon is centred inside each cell.
const CELL: i32 = 80;
const ICON_SIZE: i32 = 32;

/// Fully typed alias for the GBM-backed DRM compositor.
pub(crate) type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// Rendering state for a single CRTC/connector output.
pub struct DcsOutput {
    compositor: GbmDrmCompositor,
    /// Pre-rendered splash frame sized to this output's resolution.
    splash: MemoryRenderBuffer,
    /// Decoded RGBA bytes of the splash icon, retained for splash rebuilds on mode changes.
    icon_rgba: Vec<u8>,
    /// Set whenever the screen content needs to be (re)drawn.
    pub needs_render: bool,

    // ------------------------------------------------------------------
    // Protocol-facing display info, populated at construction time and
    // sent to clients via zwp_dcs_output events.
    // ------------------------------------------------------------------
    /// DRM connector handle for this output (used by DRM leasing).
    pub connector_handle: drm::control::connector::Handle,
    /// Modes reported by the connector at initialisation time.
    /// Cached here so mode resolution can be performed without access to
    /// the `DrmDevice` (e.g. inside [`DisplayAttribute::validate`]).
    pub connector_modes: Vec<drm::control::Mode>,
    /// Active DRM lease for this output, if any. Dropping revokes the lease.
    pub active_lease: Option<DrmLease>,
    /// 1-based display index, shown on the splash screen and sent as the
    /// `number` event.
    pub display_number: i32,
    /// Active mode width in pixels.
    pub mode_width: u32,
    /// Active mode height in pixels.
    pub mode_height: u32,
    /// Active mode refresh rate in mHz (e.g. 60000 for 60 Hz).
    pub mode_refresh_mhz: u32,
    /// DRM device number (st_rdev / dev_t, truncated to 32 bits) sent as
    /// the `device` event so clients can identify which GPU owns this output.
    pub dev_t: u32,
}

/// Build a CPU-side splash screen `(w × h)` pixels in `Fourcc::Argb8888`
/// format, with a centred grid of icons on a black background.
fn build_splash(w: i32, h: i32, icon_rgba: &[u8]) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (w * h) as usize * 4];

    // Black opaque background: [B=0, G=0, R=0, A=255]
    for p in pixels.chunks_exact_mut(4) {
        p[3] = 255;
    }

    let cols = w / CELL;
    let rows = h / CELL;
    let grid_x = (w - cols * CELL) / 2;
    let grid_y = (h - rows * CELL) / 2;
    let icon_off = (CELL - ICON_SIZE) / 2;

    for row in 0..rows {
        for col in 0..cols {
            let ox = grid_x + col * CELL + icon_off;
            let oy = grid_y + row * CELL + icon_off;

            for py in 0..ICON_SIZE {
                for px in 0..ICON_SIZE {
                    let sx = ox + px;
                    let sy = oy + py;
                    if sx < 0 || sy < 0 || sx >= w || sy >= h {
                        continue;
                    }
                    let src = (py * ICON_SIZE + px) as usize * 4;
                    let alpha = icon_rgba[src + 3];
                    if alpha == 0 {
                        continue;
                    }
                    // PNG stores [R, G, B, A]; Argb8888 buffer wants [B, G, R, A].
                    let dst = (sy * w + sx) as usize * 4;
                    pixels[dst] = icon_rgba[src + 2]; // B
                    pixels[dst + 1] = icon_rgba[src + 1]; // G
                    pixels[dst + 2] = icon_rgba[src]; // R
                    pixels[dst + 3] = alpha;
                }
            }
        }
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (w, h),
        1,
        Transform::Normal,
        None,
    )
}

/// Find a free CRTC for `connector_info` that is not already in `used_crtcs`.
///
/// Preference order:
/// 1. The CRTC the connector's active encoder is already driving.
/// 2. Any CRTC reachable via the connector's encoders, checked against each
///    encoder's `possible_crtcs` bitmask (bit N = CRTC at index N in the
///    device resource list).
fn find_crtc(
    drm_device: &DrmDevice,
    connector_info: &connector::Info,
    used_crtcs: &HashSet<crtc::Handle>,
) -> anyhow::Result<crtc::Handle> {
    let resources = drm_device.resource_handles()?;

    if let Some(enc_handle) = connector_info.current_encoder() {
        if let Ok(enc_info) = drm_device.get_encoder(enc_handle) {
            if let Some(crtc) = enc_info.crtc() {
                if !used_crtcs.contains(&crtc) {
                    return Ok(crtc);
                }
            }
        }
    }

    for &enc_handle in connector_info.encoders() {
        if let Ok(enc_info) = drm_device.get_encoder(enc_handle) {
            for crtc in resources.filter_crtcs(enc_info.possible_crtcs()) {
                if !used_crtcs.contains(&crtc) {
                    return Ok(crtc);
                }
            }
        }
    }

    anyhow::bail!(
        "No available CRTC for connector {:?}",
        connector_info.handle()
    )
}

impl DcsOutput {
    /// Create an output for the given connected connector.
    ///
    /// Returns the `crtc::Handle` claimed alongside the new output so the
    /// caller can track which CRTCs are in use when creating further outputs
    /// on the same device.
    pub(super) fn new(
        drm_device: &mut DrmDevice,
        gbm_device: gbm::Device<DrmDeviceFd>,
        render_formats: &[DrmFormat],
        connector_info: connector::Info,
        icon_rgba: &[u8],
        used_crtcs: &HashSet<crtc::Handle>,
        display_number: i32,
        dev_t: u32,
    ) -> anyhow::Result<(crtc::Handle, Self)> {
        let connector_modes: Vec<drm::control::Mode> = connector_info.modes().to_vec();
        let mode = *connector_modes
            .first()
            .context("connector has no modes")?;
        let (mw, mh) = mode.size();

        tracing::info!(
            "Connector {:?} ({:?}): {}x{}@{}Hz hsync=({},{},{}) vsync=({},{},{}) clock={}kHz",
            connector_info.handle(),
            connector_info.interface(),
            mw,
            mh,
            mode.vrefresh(),
            mode.hsync().0, mode.hsync().1, mode.hsync().2,
            mode.vsync().0, mode.vsync().1, mode.vsync().2,
            mode.clock(),
        );

        let crtc = find_crtc(drm_device, &connector_info, used_crtcs)?;
        tracing::info!("Using CRTC {:?}", crtc);

        let surface = drm_device.create_surface(crtc, mode, &[connector_info.handle()])?;

        let allocator = GbmAllocator::new(
            gbm_device.clone(),
            GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
        );
        let exporter = GbmFramebufferExporter::new(
            gbm_device.clone(),
            smithay::backend::drm::exporter::gbm::NodeFilter::None,
        );

        let output_mode = OutputModeSource::Static {
            size: Size::from((mw as i32, mh as i32)),
            scale: Scale::from(1.0),
            transform: Transform::Normal,
        };

        let compositor = DrmCompositor::new(
            output_mode,
            surface,
            None,
            allocator,
            exporter,
            [DrmFourcc::Xrgb8888, DrmFourcc::Argb8888],
            render_formats.iter().copied(),
            Size::from((64u32, 64u32)),
            Some(gbm_device),
        )
        .map_err(|e| anyhow::anyhow!("DrmCompositor::new failed: {:?}", e))?;

        let splash = build_splash(mw as i32, mh as i32, icon_rgba);

        Ok((
            crtc,
            DcsOutput {
                compositor,
                splash,
                icon_rgba: icon_rgba.to_vec(),
                needs_render: true,
                connector_handle: connector_info.handle(),
                connector_modes,
                active_lease: None,
                display_number,
                mode_width: mw as u32,
                mode_height: mh as u32,
                mode_refresh_mhz: mode.vrefresh() * 1000,
                dev_t,
            },
        ))
    }

    /// Render the splash screen if `needs_render` is set.
    ///
    /// `renderer` must belong to the same GPU as this output's compositor.
    pub fn render_with(&mut self, renderer: &mut GlesRenderer) -> anyhow::Result<()> {
        if !self.needs_render {
            return Ok(());
        }

        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0_f64, 0.0_f64),
            &self.splash,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .map_err(|e| anyhow::anyhow!("from_buffer failed: {:?}", e))?;

        let elements = [element];
        let result = self
            .compositor
            .render_frame(
                renderer,
                &elements,
                [0.0_f32, 0.0, 0.0, 1.0],
                FrameFlags::empty(),
            )
            .map_err(|e| anyhow::anyhow!("render_frame failed: {:?}", e))?;

        if !result.is_empty {
            self.compositor
                .queue_frame(())
                .map_err(|e| anyhow::anyhow!("queue_frame failed: {:?}", e))?;
            self.needs_render = false;
        }

        Ok(())
    }

    /// Return the primary plane handle and its claim for this output.
    ///
    /// Both are required by [`DrmLeaseBuilder`] alongside the connector and
    /// CRTC — atomic KMS rejects a lease that does not include the primary
    /// plane (`DRM_CLIENT_CAP_UNIVERSAL_PLANES` is enabled by smithay).
    ///
    /// The `PlaneClaim` is Arc-backed; cloning it does not release the
    /// compositor's own hold on the plane.
    pub fn primary_plane_with_claim(&self) -> Option<(drm::control::plane::Handle, PlaneClaim)> {
        let plane = self.compositor.plane();
        let claim = self.compositor.surface().claim_plane(plane)?;
        Some((plane, claim))
    }

    /// Re-sync compositor state and schedule a redraw on the next render pass.
    ///
    /// Call this after a DRM lease ends.  `reset_state()` re-reads the actual
    /// kernel CRTC/plane state so smithay's internal bookkeeping matches what
    /// the hardware is doing.  With `SET_PERSISTENT_DISPLAY` enabled the CRTC
    /// stays active (same mode, same connectors) so the subsequent render is a
    /// page flip — no full modeset, framelock state is preserved.  If the
    /// client changed the display configuration during the lease, the next
    /// render performs a modeset to restore the configured mode instead.
    pub fn request_redraw(&mut self) {
        if let Err(e) = self.compositor.reset_state() {
            tracing::warn!("Failed to reset DRM state after lease end: {}", e);
        }

        // After reset_state() the current mode reflects what the hardware is
        // actually driving, while the pending mode is our configured mode.
        // If they differ the leased client deviated from the configuration
        // and the redraw below will have to perform a modeset to restore it.
        let current = self.compositor.current_mode();
        let pending = self.compositor.pending_mode();
        if current != pending {
            tracing::warn!(
                "Leased client changed the configuration of display {} (hardware \
                 is driving {:?}, configured mode is {:?}), performing a modeset \
                 to restore the requested configuration",
                self.display_number,
                current,
                pending,
            );
        }

        self.needs_render = true;
    }

    /// Return the connector handles currently attached to this output's compositor.
    ///
    /// Used by [`super::DcsDevice`] to look up the connector's mode list when
    /// processing a mode-change request.
    pub(super) fn current_connectors(
        &self,
    ) -> impl IntoIterator<Item = drm::control::connector::Handle> + use<'_> {
        self.compositor.current_connectors()
    }

    /// Attempt to change the active mode.
    ///
    /// The new mode is first staged on the underlying `DrmSurface` and tested
    /// via an atomic test-only commit (on legacy KMS this falls back to a
    /// buffer test or is assumed to succeed). If the test fails the surface is
    /// reverted to its current mode and an error is returned. On success the
    /// mode is applied through the `DrmCompositor` (which also resizes the
    /// swapchain), the stored mode fields are updated, the splash is rebuilt at
    /// the new resolution, and `needs_render` is set.
    /// Resolve `(width, height, refresh_mhz)` to a [`drm::control::Mode`]
    /// from this output's cached connector mode list.
    fn resolve_mode_by_params(
        &self,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<drm::control::Mode> {
        self.connector_modes
            .iter()
            .find(|m| {
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

    /// Test whether the mode identified by `(width, height, refresh_mhz)` is
    /// accepted by the hardware, without applying it.
    ///
    /// Resolves the mode from the cached connector mode list, then delegates
    /// to [`test_mode_change`].
    pub fn test_mode_change_by_params(
        &self,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let mode = self.resolve_mode_by_params(width, height, refresh_mhz)?;
        self.test_mode_change(mode)
    }

    /// Apply the mode identified by `(width, height, refresh_mhz)` to this output.
    ///
    /// Resolves the mode from the cached connector mode list, then delegates
    /// to [`apply_mode_change`].
    pub fn apply_mode_change_by_params(
        &mut self,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> anyhow::Result<()> {
        let mode = self.resolve_mode_by_params(width, height, refresh_mhz)?;
        self.apply_mode_change(mode)
    }

    /// Test whether `mode` is accepted by the hardware without applying it.
    ///
    /// Stages the mode on the underlying `DrmSurface`, runs an atomic
    /// test-only commit (best-effort on legacy KMS), then always reverts the
    /// surface back to its current mode regardless of the outcome. Returns
    /// `Ok(())` if the hardware would accept the mode, or an error if not.
    pub fn test_mode_change(&self, mode: drm::control::Mode) -> anyhow::Result<()> {
        let current_mode = self.compositor.current_mode();

        // Stage the mode on the surface so test_state can evaluate it.
        self.compositor
            .surface()
            .use_mode(mode)
            .map_err(|e| anyhow::anyhow!("surface use_mode failed: {:?}", e))?;

        // Atomic test-only commit (TEST_ONLY flag; no visible changes).
        let result = self
            .compositor
            .surface()
            .test_state(std::iter::empty::<PlaneState<'_>>(), true);

        // Always revert — this is a test-only operation.
        let _ = self.compositor.surface().use_mode(current_mode);

        result.map_err(|e| anyhow::anyhow!("mode test commit rejected: {:?}", e))
    }

    /// Apply `mode` to this output.
    ///
    /// Stages the mode through the `DrmCompositor` (which also resizes the
    /// swapchain), updates the stored mode fields, rebuilds the splash at the
    /// new resolution, and sets `needs_render` so the hardware modeset is
    /// issued on the next `render_frame` call.
    pub fn apply_mode_change(&mut self, mode: drm::control::Mode) -> anyhow::Result<()> {
        self.compositor
            .use_mode(mode)
            .map_err(|e| anyhow::anyhow!("compositor use_mode failed: {:?}", e))?;

        let (mw, mh) = mode.size();
        self.mode_width = mw as u32;
        self.mode_height = mh as u32;
        self.mode_refresh_mhz = mode.vrefresh() * 1000;
        self.splash = build_splash(mw as i32, mh as i32, &self.icon_rgba);

        // Reset the compositor's internal damage state so the next render_frame
        // call sees a fully-damaged frame and issues the atomic commit that
        // applies the new mode to hardware.  Without this, render_frame may
        // return is_empty=true (no damage detected) and skip queue_frame,
        // leaving the mode change staged but never committed.
        if let Err(e) = self.compositor.reset_state() {
            tracing::warn!("failed to reset compositor state after mode change: {}", e);
        }

        self.needs_render = true;
        Ok(())
    }

    /// Notify the compositor that the queued frame has been scanned out.
    ///
    /// Must be called on every `DrmEvent::VBlank` for this output's CRTC.
    pub fn frame_submitted(&mut self) -> anyhow::Result<()> {
        self.compositor
            .frame_submitted()
            .map_err(|e| anyhow::anyhow!("frame_submitted failed: {:?}", e))?;
        Ok(())
    }
}
