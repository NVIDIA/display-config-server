//! VK_KHR_display enumeration.
//!
//! Loads the Vulkan library, creates an instance, and enumerates all displays
//! and their modes via the VK_KHR_display extension.

use std::ffi::CStr;

use ash::khr;
use ash::vk;

/// A mode reported by a Vulkan display.
#[derive(Debug, Clone)]
pub struct VkMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

/// State collected for one Vulkan display.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct VkDisplayInfo {
    pub name: String,
    pub modes: Vec<VkMode>,
}

/// All Vulkan display state collected from one physical device.
#[derive(Debug, Clone)]
pub struct VkPhysDeviceDisplays {
    pub device_name: String,
    pub displays: Vec<VkDisplayInfo>,
}

/// Enumerate all VK_KHR_display displays and modes.
///
/// Returns one entry per physical device that supports VK_KHR_display.
/// Returns `Err` if Vulkan is not available or no physical device is found.
pub fn query_vk_displays() -> Result<Vec<VkPhysDeviceDisplays>, Box<dyn std::error::Error>> {
    let entry = unsafe { ash::Entry::load() }
        .map_err(|e| format!("failed to load Vulkan: {}", e))?;

    // Check that VK_KHR_display is supported at instance level.
    let instance_extensions = unsafe { entry.enumerate_instance_extension_properties(None) }?;
    let has_display_ext = instance_extensions.iter().any(|ext| {
        let name = unsafe { CStr::from_ptr(ext.extension_name.as_ptr()) };
        name == khr::display::NAME
    });
    if !has_display_ext {
        return Err("VK_KHR_display instance extension not available".into());
    }

    let app_info = vk::ApplicationInfo::default()
        .api_version(vk::make_api_version(0, 1, 0, 0));
    let extension_names = [khr::display::NAME.as_ptr()];
    let instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&extension_names);

    let instance = unsafe { entry.create_instance(&instance_info, None) }?;
    let display_fn = khr::display::Instance::new(&entry, &instance);

    let physical_devices = unsafe { instance.enumerate_physical_devices() }?;

    let mut results = Vec::new();

    for phys in physical_devices {
        let props = unsafe { instance.get_physical_device_properties(phys) };
        let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        let display_props = unsafe {
            display_fn.get_physical_device_display_properties(phys)
        }?;

        let mut displays = Vec::new();
        for dp in &display_props {
            let name = if dp.display_name.is_null() {
                String::from("<unnamed>")
            } else {
                unsafe { CStr::from_ptr(dp.display_name) }
                    .to_string_lossy()
                    .into_owned()
            };

            let mode_props = unsafe {
                display_fn.get_display_mode_properties(phys, dp.display)
            }?;

            let modes: Vec<VkMode> = mode_props
                .iter()
                .map(|m| VkMode {
                    width: m.parameters.visible_region.width,
                    height: m.parameters.visible_region.height,
                    refresh_rate: m.parameters.refresh_rate,
                })
                .collect();

            displays.push(VkDisplayInfo { name, modes });
        }

        results.push(VkPhysDeviceDisplays {
            device_name,
            displays,
        });
    }

    unsafe { instance.destroy_instance(None) };

    Ok(results)
}
