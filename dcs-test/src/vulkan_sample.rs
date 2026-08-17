// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! `dcs-test vulkan-sample` — Vulkan D2D color-cycle client.
//!
//! Presents a smoothly-cycling color to every VK_KHR_display display,
//! optionally joining all swapchains into a VK_NV_present_barrier group.
//! Relies on the NVIDIA driver's transparent DCS lease integration — no
//! Wayland lease protocol code here.

use std::collections::HashSet;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ash::khr;
use ash::vk;

/// Options for the vulkan-sample subcommand.
pub struct SampleOptions {
    /// Exit after this long; `None` = run until Ctrl+C.
    pub duration: Option<Duration>,
    /// Enable VK_NV_present_barrier on every swapchain.
    pub present_barrier: bool,
}

/// Parse the arguments following the `vulkan-sample` subcommand.
pub fn parse_args(args: &[String]) -> Result<SampleOptions, String> {
    let mut duration = None;
    let mut present_barrier = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--duration" => {
                let value = it
                    .next()
                    .ok_or_else(|| String::from("--duration requires a value in seconds"))?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --duration '{}': expected seconds", value))?;
                duration = Some(Duration::from_secs(secs));
            }
            "--present-barrier" => present_barrier = true,
            other => return Err(format!("unknown vulkan-sample argument: {}", other)),
        }
    }
    Ok(SampleOptions { duration, present_barrier })
}

/// Seconds for one full hue sweep.  Slow enough to look smooth, fast
/// enough that out-of-sync displays are visually obvious.
pub(crate) const COLOR_PERIOD_SECS: f32 = 10.0;

/// Standard HSV → RGB (h in degrees, s/v in [0,1]).
pub(crate) fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

/// The clear color for a frame at `elapsed_secs`: a full-saturation hue
/// sweep with period [`COLOR_PERIOD_SECS`], opaque alpha.
pub(crate) fn frame_color(elapsed_secs: f32) -> [f32; 4] {
    let hue = (elapsed_secs / COLOR_PERIOD_SECS).fract() * 360.0;
    let [r, g, b] = hsv_to_rgb(hue, 1.0, 1.0);
    [r, g, b, 1.0]
}

/// Cleared by the SIGINT handler; the present loop polls it.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Signal handler: request a clean stop, then restore the default SIGINT
/// disposition. If the present loop is blocked in a driver call (e.g.
/// `acquire_next_image` with a `u64::MAX` timeout, or `queue_wait_idle`
/// stalled by a broken present-barrier group) it will never observe
/// `RUNNING`, so a second Ctrl+C must be able to terminate the process the
/// normal way instead of being swallowed by this handler again.
extern "C" fn on_sigint(_signum: i32) {
    RUNNING.store(false, Ordering::SeqCst);
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

/// One display we present to: its surface, swapchain, and sync objects.
struct DisplayTarget {
    name: String,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    acquire_sem: vk::Semaphore,
    render_sem: vk::Semaphore,
    cmd_buf: vk::CommandBuffer,
}

/// One physical device and everything created on it.
struct DeviceCtx {
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    swapchain_fn: khr::swapchain::Device,
    targets: Vec<DisplayTarget>,
}

/// Run the vulkan-sample: set up D2D surfaces on every display, create a
/// clear-only swapchain per display, and present a cycling color until
/// Ctrl+C or `--duration` elapses.
pub fn run(opts: &SampleOptions) -> Result<(), Box<dyn std::error::Error>> {
    let entry = unsafe { ash::Entry::load() }
        .map_err(|e| format!("failed to load Vulkan: {}", e))?;

    let mut instance_exts = vec![khr::surface::NAME.as_ptr(), khr::display::NAME.as_ptr()];
    if opts.present_barrier {
        instance_exts.push(khr::get_surface_capabilities2::NAME.as_ptr());
    }
    let app_info = vk::ApplicationInfo::default()
        .api_version(vk::make_api_version(0, 1, 1, 0));
    let instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&instance_exts);
    let instance = unsafe { entry.create_instance(&instance_info, None) }.map_err(|e| {
        format!(
            "failed to create Vulkan instance (is a driver with VK_KHR_display available?): {}",
            e
        )
    })?;

    let result = run_with_instance(opts, &entry, &instance);
    unsafe { instance.destroy_instance(None) };
    result
}

fn run_with_instance(
    opts: &SampleOptions,
    entry: &ash::Entry,
    instance: &ash::Instance,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_fn = khr::display::Instance::new(entry, instance);
    let surface_fn = khr::surface::Instance::new(entry, instance);

    let mut devices: Vec<DeviceCtx> = Vec::new();
    let mut total_displays = 0usize;

    for phys in unsafe { instance.enumerate_physical_devices() }? {
        match setup_device(opts, entry, instance, &display_fn, &surface_fn, phys) {
            Ok(Some(ctx)) => {
                total_displays += ctx.targets.len();
                devices.push(ctx);
            }
            Ok(None) => {}
            Err(e) => {
                cleanup(&surface_fn, instance, &mut devices);
                return Err(e);
            }
        }
    }

    if total_displays == 0 {
        cleanup(&surface_fn, instance, &mut devices);
        return Err("no VK_KHR_display displays found — is DCS leasing them?".into());
    }

    // --- present loop ---
    unsafe { libc::signal(libc::SIGINT, on_sigint as extern "C" fn(i32) as usize) };
    println!(
        "vulkan-sample: cycling color on {} display(s){} — Ctrl+C to stop \
         (press twice if a stalled present barrier keeps the first from taking effect)",
        total_displays,
        if opts.present_barrier { " with present barrier" } else { "" }
    );

    let start = Instant::now();
    let loop_result = present_loop(opts, start, &devices);

    cleanup(&surface_fn, instance, &mut devices);
    loop_result
}

/// Objects created so far while setting up one physical device.  On a
/// setup error they are destroyed in reverse creation order; on success
/// they are moved into the returned [`DeviceCtx`].
#[derive(Default)]
struct PartialDevice {
    /// Surfaces not yet owned by a [`DisplayTarget`].
    surfaces: Vec<(String, vk::SurfaceKHR)>,
    device: Option<ash::Device>,
    swapchain_fn: Option<khr::swapchain::Device>,
    cmd_pool: vk::CommandPool,
    targets: Vec<DisplayTarget>,
}

impl PartialDevice {
    /// Destroy everything recorded so far (setup failed partway).
    /// Null handles inside targets are ignored by the Vulkan destroy calls.
    fn destroy(&mut self, surface_fn: &khr::surface::Instance) {
        if let Some(device) = self.device.take() {
            let swapchain_fn = self.swapchain_fn.take();
            unsafe {
                let _ = device.device_wait_idle();
                for target in self.targets.drain(..) {
                    device.destroy_semaphore(target.acquire_sem, None);
                    device.destroy_semaphore(target.render_sem, None);
                    if let Some(swapchain_fn) = &swapchain_fn {
                        swapchain_fn.destroy_swapchain(target.swapchain, None);
                    }
                    surface_fn.destroy_surface(target.surface, None);
                }
                device.destroy_command_pool(self.cmd_pool, None);
                self.cmd_pool = vk::CommandPool::null();
                device.destroy_device(None);
            }
        }
        for (_, surface) in self.surfaces.drain(..) {
            unsafe { surface_fn.destroy_surface(surface, None) };
        }
    }
}

/// Set up one physical device end-to-end: display surfaces, logical
/// device, and clear-only swapchains.  Returns `Ok(None)` if the device
/// has no displays.  On error, every Vulkan object created for this
/// device is destroyed before returning.
fn setup_device(
    opts: &SampleOptions,
    entry: &ash::Entry,
    instance: &ash::Instance,
    display_fn: &khr::display::Instance,
    surface_fn: &khr::surface::Instance,
    phys: vk::PhysicalDevice,
) -> Result<Option<DeviceCtx>, Box<dyn std::error::Error>> {
    let mut partial = PartialDevice::default();
    let result =
        try_setup_device(opts, entry, instance, display_fn, surface_fn, phys, &mut partial);
    if result.is_err() {
        partial.destroy(surface_fn);
    }
    result
}

/// The fallible body of [`setup_device`].  Every created object is
/// recorded in `partial` as soon as it exists so [`setup_device`] can
/// destroy it if a later step fails.
fn try_setup_device(
    opts: &SampleOptions,
    entry: &ash::Entry,
    instance: &ash::Instance,
    display_fn: &khr::display::Instance,
    surface_fn: &khr::surface::Instance,
    phys: vk::PhysicalDevice,
    partial: &mut PartialDevice,
) -> Result<Option<DeviceCtx>, Box<dyn std::error::Error>> {
    let props = unsafe { instance.get_physical_device_properties(phys) };
    let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // --- surfaces: one per display, on the display's current mode ---
    let display_props = unsafe { display_fn.get_physical_device_display_properties(phys) }?;
    if display_props.is_empty() {
        return Ok(None);
    }
    let plane_props = unsafe { display_fn.get_physical_device_display_plane_properties(phys) }?;
    // Planes already assigned to an earlier display in this loop — some
    // hardware reports a plane as supporting multiple displays, but each
    // plane can only drive one display at a time.
    let mut claimed_planes: HashSet<u32> = HashSet::new();

    for dp in &display_props {
        let name = if dp.display_name.is_null() {
            format!("{} display", device_name)
        } else {
            unsafe { CStr::from_ptr(dp.display_name) }
                .to_string_lossy()
                .into_owned()
        };

        let modes = unsafe { display_fn.get_display_mode_properties(phys, dp.display) }?;
        let mode = modes
            .first()
            .ok_or_else(|| format!("{}: no display modes", name))?;

        // Find a plane that supports this display and isn't already claimed
        // by an earlier display on this device. Prefer a plane whose
        // current_display already matches (or is unset), falling back to
        // any other unclaimed, supporting plane.
        let mut plane_index = None;
        let mut fallback_index = None;
        for (i, pp) in plane_props.iter().enumerate() {
            let i = i as u32;
            if claimed_planes.contains(&i) {
                continue;
            }
            let supported =
                unsafe { display_fn.get_display_plane_supported_displays(phys, i) }?;
            if !supported.contains(&dp.display) {
                continue;
            }
            if pp.current_display == dp.display || pp.current_display == vk::DisplayKHR::null() {
                plane_index = Some(i);
                break;
            }
            if fallback_index.is_none() {
                fallback_index = Some(i);
            }
        }
        let plane_index = plane_index.or(fallback_index).ok_or_else(|| {
            format!("{}: no compatible display plane available (all claimed by other displays on this GPU)", name)
        })?;
        claimed_planes.insert(plane_index);

        let surface_info = vk::DisplaySurfaceCreateInfoKHR::default()
            .display_mode(mode.display_mode)
            .plane_index(plane_index)
            .plane_stack_index(0)
            .transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .alpha_mode(vk::DisplayPlaneAlphaFlagsKHR::OPAQUE)
            .image_extent(mode.parameters.visible_region);
        let surface = unsafe { display_fn.create_display_plane_surface(&surface_info, None) }?;
        partial.surfaces.push((name, surface));
    }

    // --- present-barrier support checks (before device creation) ---
    if opts.present_barrier {
        let dev_exts = unsafe { instance.enumerate_device_extension_properties(phys) }?;
        let has_pb = dev_exts.iter().any(|e| {
            (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) })
                == ash::nv::present_barrier::NAME
        });
        if !has_pb {
            return Err(format!(
                "{}: VK_NV_present_barrier not supported (use without --present-barrier)",
                device_name
            )
            .into());
        }
        let gsc2_fn = khr::get_surface_capabilities2::Instance::new(entry, instance);
        for (name, surface) in &partial.surfaces {
            let mut pb_caps = vk::SurfaceCapabilitiesPresentBarrierNV::default();
            let mut caps2 = vk::SurfaceCapabilities2KHR::default().push_next(&mut pb_caps);
            let info = vk::PhysicalDeviceSurfaceInfo2KHR::default().surface(*surface);
            unsafe {
                gsc2_fn.get_physical_device_surface_capabilities2(phys, &info, &mut caps2)
            }?;
            if pb_caps.present_barrier_supported == vk::FALSE {
                return Err(format!(
                    "{}: present barrier not supported on this surface \
                     (use without --present-barrier)",
                    name
                )
                .into());
            }
        }
    }

    // --- queue family: graphics + present support for every surface ---
    let queue_props = unsafe { instance.get_physical_device_queue_family_properties(phys) };
    let mut family = None;
    'families: for (i, qp) in queue_props.iter().enumerate() {
        if !qp.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
            continue;
        }
        for (_, surface) in &partial.surfaces {
            let ok = unsafe {
                surface_fn.get_physical_device_surface_support(phys, i as u32, *surface)
            }?;
            if !ok {
                continue 'families;
            }
        }
        family = Some(i as u32);
        break;
    }
    let family = family.ok_or_else(|| {
        format!("{}: no graphics queue family can present to all displays", device_name)
    })?;

    // --- logical device ---
    let queue_prio = [1.0f32];
    let queue_info = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family)
        .queue_priorities(&queue_prio)];
    let mut dev_exts = vec![khr::swapchain::NAME.as_ptr()];
    let mut pb_features =
        vk::PhysicalDevicePresentBarrierFeaturesNV::default().present_barrier(true);
    let mut device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_info);
    if opts.present_barrier {
        dev_exts.push(ash::nv::present_barrier::NAME.as_ptr());
        device_info = device_info.push_next(&mut pb_features);
    }
    device_info = device_info.enabled_extension_names(&dev_exts);
    let device = unsafe { instance.create_device(phys, &device_info, None) }?;
    let queue = unsafe { device.get_device_queue(family, 0) };
    partial.swapchain_fn = Some(khr::swapchain::Device::new(instance, &device));
    partial.device = Some(device);
    let device = partial.device.as_ref().unwrap();
    let swapchain_fn = partial.swapchain_fn.as_ref().unwrap();

    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    partial.cmd_pool = unsafe { device.create_command_pool(&pool_info, None) }?;

    // --- swapchain + sync objects per display ---
    while !partial.surfaces.is_empty() {
        let surface = partial.surfaces[0].1;
        let caps = unsafe {
            surface_fn.get_physical_device_surface_capabilities(phys, surface)
        }?;
        let formats = unsafe { surface_fn.get_physical_device_surface_formats(phys, surface) }?;
        let format = formats
            .iter()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .copied()
            .unwrap_or(formats[0]);

        let mut pb_info =
            vk::SwapchainPresentBarrierCreateInfoNV::default().present_barrier_enable(true);
        let mut swap_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(caps.min_image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(caps.current_extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true);
        if opts.present_barrier {
            swap_info = swap_info.push_next(&mut pb_info);
        }
        let swapchain = unsafe { swapchain_fn.create_swapchain(&swap_info, None) }?;

        // The swapchain exists now: move it and its surface into a target
        // immediately so an error below still destroys them.
        let (name, surface) = partial.surfaces.remove(0);
        partial.targets.push(DisplayTarget {
            name,
            surface,
            swapchain,
            images: Vec::new(),
            acquire_sem: vk::Semaphore::null(),
            render_sem: vk::Semaphore::null(),
            cmd_buf: vk::CommandBuffer::null(),
        });
        let target = partial.targets.last_mut().unwrap();
        target.images = unsafe { swapchain_fn.get_swapchain_images(swapchain) }?;

        let sem_info = vk::SemaphoreCreateInfo::default();
        target.acquire_sem = unsafe { device.create_semaphore(&sem_info, None) }?;
        target.render_sem = unsafe { device.create_semaphore(&sem_info, None) }?;

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(partial.cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        target.cmd_buf = unsafe { device.allocate_command_buffers(&alloc_info) }?[0];

        println!("vulkan-sample: presenting to {} ({})", target.name, device_name);
    }

    Ok(Some(DeviceCtx {
        device: partial.device.take().unwrap(),
        queue,
        cmd_pool: std::mem::take(&mut partial.cmd_pool),
        swapchain_fn: partial.swapchain_fn.take().unwrap(),
        targets: std::mem::take(&mut partial.targets),
    }))
}

fn present_loop(
    opts: &SampleOptions,
    start: Instant,
    devices: &[DeviceCtx],
) -> Result<(), Box<dyn std::error::Error>> {
    while RUNNING.load(Ordering::SeqCst) {
        let elapsed = start.elapsed();
        if let Some(limit) = opts.duration {
            if elapsed >= limit {
                break;
            }
        }
        let color = frame_color(elapsed.as_secs_f32());

        for dev in devices {
            for target in &dev.targets {
                present_one(dev, target, color)
                    .map_err(|e| format!("{}: {}", target.name, e))?;
            }
            // Simplicity over throughput: idle the queue each frame so the
            // single command buffer and semaphores can be reused safely.
            unsafe { dev.device.queue_wait_idle(dev.queue) }?;
        }
    }
    Ok(())
}

fn present_one(
    dev: &DeviceCtx,
    target: &DisplayTarget,
    color: [f32; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let (image_index, _suboptimal) = unsafe {
        dev.swapchain_fn.acquire_next_image(
            target.swapchain,
            u64::MAX,
            target.acquire_sem,
            vk::Fence::null(),
        )
    }?;
    let image = target.images[image_index as usize];

    let begin = vk::CommandBufferBeginInfo::default()
        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { dev.device.begin_command_buffer(target.cmd_buf, &begin) }?;

    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);

    let to_transfer = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range);
    unsafe {
        dev.device.cmd_pipeline_barrier(
            target.cmd_buf,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer],
        );
        dev.device.cmd_clear_color_image(
            target.cmd_buf,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { float32: color },
            &[range],
        );
    }
    let to_present = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::empty())
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range);
    unsafe {
        dev.device.cmd_pipeline_barrier(
            target.cmd_buf,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_present],
        );
        dev.device.end_command_buffer(target.cmd_buf)?;
    }

    let wait_sems = [target.acquire_sem];
    let wait_stages = [vk::PipelineStageFlags::TRANSFER];
    let cmd_bufs = [target.cmd_buf];
    let signal_sems = [target.render_sem];
    let submit = vk::SubmitInfo::default()
        .wait_semaphores(&wait_sems)
        .wait_dst_stage_mask(&wait_stages)
        .command_buffers(&cmd_bufs)
        .signal_semaphores(&signal_sems);
    unsafe { dev.device.queue_submit(dev.queue, &[submit], vk::Fence::null()) }?;

    let swapchains = [target.swapchain];
    let indices = [image_index];
    let present_wait = [target.render_sem];
    let present = vk::PresentInfoKHR::default()
        .wait_semaphores(&present_wait)
        .swapchains(&swapchains)
        .image_indices(&indices);
    unsafe { dev.swapchain_fn.queue_present(dev.queue, &present) }?;
    Ok(())
}

fn cleanup(
    surface_fn: &khr::surface::Instance,
    _instance: &ash::Instance,
    devices: &mut Vec<DeviceCtx>,
) {
    for dev in devices.drain(..) {
        unsafe {
            let _ = dev.device.device_wait_idle();
            for target in &dev.targets {
                dev.device.destroy_semaphore(target.acquire_sem, None);
                dev.device.destroy_semaphore(target.render_sem, None);
                dev.swapchain_fn.destroy_swapchain(target.swapchain, None);
                surface_fn.destroy_surface(target.surface, None);
            }
            dev.device.destroy_command_pool(dev.cmd_pool, None);
            dev.device.destroy_device(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn strs(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_defaults() {
        let opts = parse_args(&[]).unwrap();
        assert_eq!(opts.duration, None);
        assert!(!opts.present_barrier);
    }

    #[test]
    fn parse_duration_and_barrier() {
        let opts = parse_args(&strs(&["--duration", "30", "--present-barrier"])).unwrap();
        assert_eq!(opts.duration, Some(Duration::from_secs(30)));
        assert!(opts.present_barrier);
    }

    #[test]
    fn parse_duration_missing_value() {
        assert!(parse_args(&strs(&["--duration"])).is_err());
    }

    #[test]
    fn parse_duration_non_numeric() {
        assert!(parse_args(&strs(&["--duration", "soon"])).is_err());
    }

    #[test]
    fn parse_unknown_flag() {
        assert!(parse_args(&strs(&["--frobnicate"])).is_err());
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [1.0, 0.0, 0.0]);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), [0.0, 1.0, 0.0]);
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn frame_color_in_range_opaque() {
        for i in 0..100 {
            let c = frame_color(i as f32 * 0.37);
            for ch in &c[..3] {
                assert!((0.0..=1.0).contains(ch), "channel out of range: {c:?}");
            }
            assert_eq!(c[3], 1.0);
        }
    }

    #[test]
    fn frame_color_cycles_smoothly() {
        // Colors a quarter-period apart differ; colors a full period apart match.
        let a = frame_color(0.0);
        let b = frame_color(COLOR_PERIOD_SECS / 4.0);
        assert_ne!(a, b);
        let c = frame_color(COLOR_PERIOD_SECS);
        for (x, y) in a.iter().zip(c.iter()) {
            assert!((x - y).abs() < 1e-4);
        }
    }
}
