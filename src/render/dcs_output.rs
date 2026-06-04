use std::collections::HashSet;

use anyhow::Context;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags},
            Fourcc,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd,
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
use drm::control::{connector, crtc, Device as ControlDevice};
use drm_fourcc::{DrmFormat, DrmFourcc};

/// Grid cell size in pixels. The icon is centred inside each cell.
const CELL: i32 = 80;
const ICON_SIZE: i32 = 32;

/// Fully typed alias for the GBM-backed DRM compositor.
pub(crate) type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Rendering state for a single CRTC/connector output.
pub struct DcsOutput {
    compositor: GbmDrmCompositor,
    /// Pre-rendered splash frame sized to this output's resolution.
    splash: MemoryRenderBuffer,
    /// Set whenever the screen content needs to be (re)drawn.
    pub needs_render: bool,
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

    MemoryRenderBuffer::from_slice(&pixels, Fourcc::Argb8888, (w, h), 1, Transform::Normal, None)
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
    ) -> anyhow::Result<(crtc::Handle, Self)> {
        let mode = *connector_info
            .modes()
            .first()
            .context("connector has no modes")?;
        let (mw, mh) = mode.size();

        tracing::info!(
            "Connector {:?} ({:?}): {}x{}@{}Hz",
            connector_info.handle(),
            connector_info.interface(),
            mw,
            mh,
            mode.vrefresh(),
        );

        let crtc = find_crtc(drm_device, &connector_info, used_crtcs)?;
        tracing::info!("Using CRTC {:?}", crtc);

        let surface = drm_device.create_surface(crtc, mode, &[connector_info.handle()])?;

        let allocator = GbmAllocator::new(
            gbm_device.clone(),
            GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
        );
        let exporter = GbmFramebufferExporter::new(gbm_device.clone(), None);

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

        Ok((crtc, DcsOutput { compositor, splash, needs_render: true }))
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
