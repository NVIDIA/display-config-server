use std::os::unix::io::OwnedFd;

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
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier,
        },
        egl::{EGLContext, EGLDisplay},
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Kind,
            },
            gles::GlesRenderer,
        },
    },
    output::OutputModeSource,
    utils::{DeviceFd, Scale, Size, Transform},
};
use drm::control::{connector, Device as ControlDevice};
use drm_fourcc::DrmFourcc;

/// The 32×32 RGBA PNG icon, embedded at compile time so the binary has no
/// dependency on its working directory.
const ICON_PNG: &[u8] = include_bytes!("../assets/plus_icon.png");

/// Grid cell size in pixels.  The 32×32 icon is centred inside each cell,
/// giving equal spacing between icons in both axes.
const CELL: i32 = 80;
const ICON_SIZE: i32 = 32;

/// Fully typed alias for the compositor: GBM allocation + export, no per-frame
/// user data, GBM device fd.
pub type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Owns all rendering resources for a single DRM output and drives the
/// damage-tracked render loop.
pub struct DcsOutput {
    compositor: GbmDrmCompositor,
    renderer: GlesRenderer,
    /// Pre-rendered splash frame – black background, icon grid on top.
    splash: MemoryRenderBuffer,
    /// Set to `true` whenever the screen content needs to be (re)drawn.
    pub needs_render: bool,
}

/// Decode the embedded PNG and return raw RGBA8 bytes, row-major.
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

/// Build a CPU-side splash screen `(w × h)` pixels in `Fourcc::Argb8888`
/// format ([B, G, R, A] per pixel, little-endian).
///
/// The icon grid is centred on the screen.  Each icon occupies one `CELL×CELL`
/// square so the horizontal and vertical spacing between icons is uniform:
/// `CELL - ICON_SIZE` pixels of gap between adjacent icon edges.
fn build_splash(w: i32, h: i32, icon_rgba: &[u8]) -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (w * h) as usize * 4];

    // Black opaque background: [B=0, G=0, R=0, A=255]
    for p in pixels.chunks_exact_mut(4) {
        p[3] = 255;
    }

    let cols = w / CELL;
    let rows = h / CELL;

    // Centre the grid on the screen.
    let grid_x = (w - cols * CELL) / 2;
    let grid_y = (h - rows * CELL) / 2;

    // Offset of the icon inside each cell so it's centred.
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
                        continue; // transparent – leave background colour
                    }

                    // PNG stores [R, G, B, A]; Argb8888 buffer wants [B, G, R, A].
                    let dst = (sy * w + sx) as usize * 4;
                    pixels[dst]     = icon_rgba[src + 2]; // B
                    pixels[dst + 1] = icon_rgba[src + 1]; // G
                    pixels[dst + 2] = icon_rgba[src];     // R
                    pixels[dst + 3] = alpha;
                }
            }
        }
    }

    MemoryRenderBuffer::from_slice(&pixels, Fourcc::Argb8888, (w, h), 1, Transform::Normal, None)
}

impl DcsOutput {
    /// Open `drm_path`, enumerate connectors, build the full rendering stack
    /// (GBM → EGL → GLES → DrmCompositor), pre-render the splash screen, and
    /// return the output together with the `DrmDeviceNotifier` that the caller
    /// must register with calloop to receive VBlank events.
    pub fn new(drm_path: &str) -> anyhow::Result<(Self, DrmDeviceNotifier)> {
        // ------------------------------------------------------------------ //
        // DRM device
        // ------------------------------------------------------------------ //
        let drm_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(drm_path)?;
        // DrmDeviceFd is Arc-backed – clone before DrmDevice::new consumes it.
        let drm_fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(drm_file)));
        let gbm_fd = drm_fd.clone();

        let (mut drm_device, drm_notifier) = DrmDevice::new(drm_fd, false)?;

        // ------------------------------------------------------------------ //
        // GBM → EGL → GLES renderer
        // ------------------------------------------------------------------ //
        let gbm_device = gbm::Device::new(gbm_fd)?;

        // SAFETY: no other thread holds the GBM device at this point.
        let egl_display = unsafe { EGLDisplay::new(gbm_device.clone()) }?;
        let egl_context = EGLContext::new(&egl_display)?;

        // SAFETY: the EGLContext is not current in any other thread.
        let renderer = unsafe { GlesRenderer::new(egl_context) }?;

        // ------------------------------------------------------------------ //
        // Connector / CRTC / mode selection
        // ------------------------------------------------------------------ //
        let resources = drm_device.resource_handles()?;

        let connector_info = resources
            .connectors()
            .iter()
            .map(|&c| drm_device.get_connector(c, false))
            .filter_map(Result::ok)
            .find(|c| c.state() == connector::State::Connected)
            .context("No connected DRM connector found")?;

        tracing::info!(
            "Using connector: {:?} ({:?})",
            connector_info.handle(),
            connector_info.interface()
        );

        let mode = *connector_info
            .modes()
            .first()
            .context("Connector has no modes")?;

        let (mw, mh) = mode.size();
        tracing::info!("Mode: {}x{}@{}Hz", mw, mh, mode.vrefresh());

        // Prefer the CRTC the encoder is already driving; fall back to any
        // CRTC reachable via the connector's encoders, then the first in the
        // resource list.
        let crtc = connector_info
            .current_encoder()
            .and_then(|enc| drm_device.get_encoder(enc).ok())
            .and_then(|enc| enc.crtc())
            .or_else(|| {
                connector_info
                    .encoders()
                    .iter()
                    .find_map(|&enc| drm_device.get_encoder(enc).ok()?.crtc())
            })
            .or_else(|| resources.crtcs().first().copied())
            .context("No CRTC available for connector")?;

        tracing::info!("Using CRTC: {:?}", crtc);

        // ------------------------------------------------------------------ //
        // DRM surface
        // ------------------------------------------------------------------ //
        let surface = drm_device.create_surface(crtc, mode, &[connector_info.handle()])?;

        // ------------------------------------------------------------------ //
        // Allocator + framebuffer exporter (both GBM-backed)
        // ------------------------------------------------------------------ //
        let allocator = GbmAllocator::new(
            gbm_device.clone(),
            GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
        );
        let exporter = GbmFramebufferExporter::new(gbm_device.clone(), None);

        // Formats the renderer can import via DMABUF – used by DrmCompositor
        // to negotiate the swapchain pixel format.
        let renderer_formats = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect::<Vec<_>>();

        // ------------------------------------------------------------------ //
        // DrmCompositor
        // ------------------------------------------------------------------ //
        let output_mode = OutputModeSource::Static {
            size: Size::from((mw as i32, mh as i32)),
            scale: Scale::from(1.0),
            transform: Transform::Normal,
        };

        let compositor = DrmCompositor::new(
            output_mode,
            surface,
            None, // plane assignment – let smithay decide
            allocator,
            exporter,
            [DrmFourcc::Xrgb8888, DrmFourcc::Argb8888],
            renderer_formats,
            Size::from((64u32, 64u32)), // cursor size
            Some(gbm_device),
        )
        .map_err(|e| anyhow::anyhow!("DrmCompositor::new failed: {:?}", e))?;

        // ------------------------------------------------------------------ //
        // Splash screen – pre-rendered once at startup
        // ------------------------------------------------------------------ //
        let icon_rgba = decode_icon()?;
        let splash = build_splash(mw as i32, mh as i32, &icon_rgba);

        Ok((
            DcsOutput {
                compositor,
                renderer,
                splash,
                needs_render: true,
            },
            drm_notifier,
        ))
    }

    /// Must be called on every VBlank event to let the compositor retire the
    /// submitted buffer and prepare for the next frame.
    pub fn frame_submitted(&mut self) -> anyhow::Result<()> {
        self.compositor
            .frame_submitted()
            .map_err(|e| anyhow::anyhow!("frame_submitted failed: {:?}", e))?;
        Ok(())
    }

    /// Submit a frame if `needs_render` is set.  When nothing has changed the
    /// GPU stays completely idle.
    pub fn render(&mut self) -> anyhow::Result<()> {
        if !self.needs_render {
            return Ok(());
        }

        let element = MemoryRenderBufferRenderElement::from_buffer(
            &mut self.renderer,
            (0.0_f64, 0.0_f64),
            &self.splash,
            None, // alpha (default 1.0)
            None, // src rect
            None, // size override
            Kind::Unspecified,
        )
        .map_err(|e| anyhow::anyhow!("from_buffer failed: {:?}", e))?;

        let elements = [element];
        let result = self
            .compositor
            .render_frame(
                &mut self.renderer,
                &elements,
                [0.0_f32, 0.0, 0.0, 1.0], // black (behind the splash buffer)
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
}
