// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! `dcs-test vulkan-sample` — Vulkan D2D color-cycle client.
//!
//! Presents a smoothly-cycling color to every VK_KHR_display display,
//! optionally joining all swapchains into a VK_NV_present_barrier group.
//! Relies on the NVIDIA driver's transparent DCS lease integration — no
//! Wayland lease protocol code here.

use std::time::Duration;

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

/// Run the vulkan-sample: enumerate displays and present the color cycle.
///
/// This first version validates the D2D enumeration path; the present
/// loop lands in the next commit.
pub fn run(opts: &SampleOptions) -> Result<(), Box<dyn std::error::Error>> {
    let _ = opts.present_barrier;
    let phys_devices = crate::vulkan::query_vk_displays()?;
    let total: usize = phys_devices.iter().map(|d| d.displays.len()).sum();
    if total == 0 {
        return Err("no VK_KHR_display displays found — is DCS leasing them?".into());
    }
    for dev in &phys_devices {
        for display in &dev.displays {
            println!("vulkan-sample: {} — {}", dev.device_name, display.name);
        }
    }
    Ok(())
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
