use std::collections::{HashMap, HashSet};
use std::os::unix::io::OwnedFd;

use smithay::{
    backend::{
        drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier},
        egl::{EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    },
    utils::DeviceFd,
};
use drm::control::{connector, crtc, Device as ControlDevice};
use drm_fourcc::DrmFormat;

use super::dcs_output::DcsOutput;

/// The 32×32 RGBA PNG splash icon, embedded at compile time.
const ICON_PNG: &[u8] = include_bytes!("../../assets/plus_icon.png");

/// All rendering resources belonging to a single DRM device (GPU).
///
/// One `DcsDevice` is created per GPU. It owns the `GlesRenderer` that is
/// shared across every output on that GPU, and a map of [`DcsOutput`]
/// instances keyed by `crtc::Handle` for O(1) VBlank dispatch.
pub struct DcsDevice {
    // Kept alive to hold the DRM file descriptor open for the lifetime of the device.
    #[allow(dead_code)]
    drm_device: DrmDevice,
    /// Shared renderer for all outputs on this GPU.
    renderer: GlesRenderer,
    /// Active outputs keyed by the CRTC they drive.
    pub outputs: HashMap<crtc::Handle, DcsOutput>,
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
    pub fn new(drm_path: &str) -> anyhow::Result<(Self, DrmDeviceNotifier)> {
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
        let resources = drm_device.resource_handles()?;
        let mut outputs: HashMap<crtc::Handle, DcsOutput> = HashMap::new();
        let mut used_crtcs: HashSet<crtc::Handle> = HashSet::new();

        for &connector_handle in resources.connectors() {
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
            ) {
                Ok((crtc, output)) => {
                    used_crtcs.insert(crtc);
                    outputs.insert(crtc, output);
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

        Ok((DcsDevice { drm_device, renderer, outputs }, drm_notifier))
    }

    /// Render all outputs that have `needs_render` set, skipping idle ones.
    pub fn render(&mut self) -> anyhow::Result<()> {
        for output in self.outputs.values_mut() {
            output.render_with(&mut self.renderer)?;
        }
        Ok(())
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
