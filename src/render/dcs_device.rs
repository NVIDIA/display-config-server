use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
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

use super::dcs_output::DcsOutput;

/// The 32×32 RGBA PNG splash icon, embedded at compile time.
const ICON_PNG: &[u8] = include_bytes!("../../assets/plus_icon.png");

// ---------------------------------------------------------------------------
// DRM_IOCTL_NVIDIA_SET_PERSISTENT_DISPLAY — nvidia-drm ioctl 0x1f
//
// Tells nvidia-drm to skip tearing down the hardware (disable_all +
// releaseOwnership) when a DRM lessee drops its lease.  The CRTC, plane, and
// scanout buffer stay alive so the primary master (DCS) can resume page
// flipping without a modeset.  Framelock / genlock state in NVKMS is also
// preserved because releaseOwnership is never called for lessees.
// ---------------------------------------------------------------------------

#[repr(C)]
struct SetPersistentDisplayParams {
    enable: u32,
    __pad: u32,
}

/// Build the ioctl request number for DRM_IOCTL_NVIDIA_SET_PERSISTENT_DISPLAY.
///
/// DRM_IOW('d', DRM_COMMAND_BASE + 0x1f, struct params)
/// = _IOW('d', 0x5f, 8 bytes)
///
/// _IOW encodes: direction=WRITE(1), size=8, type='d', nr=0x5f
const fn nvidia_set_persistent_display_ioctl() -> libc::c_ulong {
    const IOC_WRITE: libc::c_ulong = 1;
    const IOC_NRSHIFT: libc::c_ulong = 0;
    const IOC_TYPESHIFT: libc::c_ulong = 8;
    const IOC_SIZESHIFT: libc::c_ulong = 16;
    const IOC_DIRSHIFT: libc::c_ulong = 30;
    const NR: libc::c_ulong = 0x40 + 0x1f; // DRM_COMMAND_BASE + DRM_NVIDIA_SET_PERSISTENT_DISPLAY
    const TYPE: libc::c_ulong = b'd' as libc::c_ulong;
    const SIZE: libc::c_ulong = std::mem::size_of::<SetPersistentDisplayParams>() as libc::c_ulong;

    (IOC_WRITE << IOC_DIRSHIFT)
        | (TYPE << IOC_TYPESHIFT)
        | (NR << IOC_NRSHIFT)
        | (SIZE << IOC_SIZESHIFT)
}

unsafe fn set_persistent_display(
    fd: std::os::unix::io::RawFd,
    enable: bool,
) -> std::io::Result<()> {
    let params = SetPersistentDisplayParams {
        enable: if enable { 1 } else { 0 },
        __pad: 0,
    };
    let ret = unsafe { libc::ioctl(fd, nvidia_set_persistent_display_ioctl(), &params) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

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
        // Enable persistent-display mode so nvidia-drm preserves CRTC and
        // framelock state when a DRM lessee drops its lease.
        // ------------------------------------------------------------------ //
        match unsafe { set_persistent_display(drm_device.device_fd().as_raw_fd(), true) } {
            Ok(()) => tracing::info!(
                "Persistent display mode enabled; CRTC and framelock state \
                 will be preserved across lease revocation"
            ),
            Err(e) => tracing::warn!(
                "SET_PERSISTENT_DISPLAY not supported by driver ({}); \
                 lease revocation will require a full modeset which resets framelock",
                e
            ),
        }

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
        let dev_t = std::fs::metadata(drm_path)?.rdev() as u32;
        let resources = drm_device.resource_handles()?;
        let mut outputs: HashMap<crtc::Handle, DcsOutput> = HashMap::new();
        let mut used_crtcs: HashSet<crtc::Handle> = HashSet::new();
        let mut display_number: i32 = 1;

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

        Ok((
            DcsDevice {
                drm_device,
                drm_node,
                renderer,
                outputs,
                drm_lease_state: None,
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
