// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! dcs-test — test tool for the Display Config Server.
//!
//! Connects to a running DCS instance and verifies display/mode enumeration.
//!
//! # Modes
//!
//! - **Protocol-only** (`--protocol-only`): verifies the DCS Wayland protocol
//!   globals are advertised correctly.
//!
//! - **Vulkan** (default): additionally enumerates displays via
//!   VK_KHR_display and cross-checks the modes against the DCS protocol.
//!   Falls back to protocol-only if Vulkan is not available.
//!
//! - **vulkan-sample**: Present a color cycle to all displays (Vulkan D2D).

mod dcs_protocol;
mod vulkan;
mod vulkan_sample;

use std::process::ExitCode;

struct TestResults {
    passed: u32,
    failed: u32,
}

impl TestResults {
    fn new() -> Self {
        TestResults { passed: 0, failed: 0 }
    }

    fn pass(&mut self, name: &str) {
        println!("[PASS] {}", name);
        self.passed += 1;
    }

    fn fail(&mut self, name: &str, reason: &str) {
        println!("[FAIL] {} — {}", name, reason);
        self.failed += 1;
    }

    fn skip(&mut self, name: &str, reason: &str) {
        println!("[SKIP] {} — {}", name, reason);
    }

    fn summary(&self) -> ExitCode {
        println!("---");
        println!("{} passed, {} failed", self.passed, self.failed);
        if self.failed > 0 {
            ExitCode::from(1)
        } else {
            ExitCode::from(0)
        }
    }
}

fn run_protocol_tests(results: &mut TestResults) -> Option<dcs_protocol::DcsProtocolState> {
    let state = match dcs_protocol::query_dcs() {
        Ok(s) => s,
        Err(e) => {
            results.fail("Protocol: connect to DCS", &e.to_string());
            return None;
        }
    };
    results.pass("Protocol: connect to DCS");

    if state.manager_found {
        results.pass("Protocol: zwp_dcs_manager global advertised");
    } else {
        results.fail("Protocol: zwp_dcs_manager global advertised",
                     "global not found in registry");
    }

    if state.drm_lease_found {
        results.pass("Protocol: wp_drm_lease_device_v1 global advertised");
    } else {
        // Not a failure in headless/vkms mode — DRM leasing is optional.
        results.skip("Protocol: wp_drm_lease_device_v1 global advertised",
                     "not present (expected in headless mode)");
    }

    Some(state)
}

fn run_vulkan_tests(results: &mut TestResults) -> Option<Vec<vulkan::VkPhysDeviceDisplays>> {
    let phys_devices = match vulkan::query_vk_displays() {
        Ok(devs) => devs,
        Err(e) => {
            results.skip("Vulkan: load VK_KHR_display", &e.to_string());
            return None;
        }
    };
    results.pass("Vulkan: load VK_KHR_display");

    if phys_devices.is_empty() {
        results.fail("Vulkan: enumerate physical devices", "no devices found");
        return None;
    }

    let total_displays: usize = phys_devices.iter().map(|d| d.displays.len()).sum();

    if total_displays == 0 {
        results.fail("Vulkan: enumerate displays", "no displays found");
        return Some(phys_devices);
    }
    results.pass(&format!("Vulkan: {} display(s) found across {} device(s)",
                          total_displays, phys_devices.len()));

    for dev in &phys_devices {
        println!("       Vulkan: {}", dev.device_name);
        for (i, display) in dev.displays.iter().enumerate() {
            if display.modes.is_empty() {
                results.fail(
                    &format!("Vulkan: {} display {} ({})", dev.device_name, i, display.name),
                    "no modes",
                );
                continue;
            }

            results.pass(&format!("Vulkan: {} display {} ({}):",
                                  dev.device_name, i, display.name));
            println!("       Vulkan:   {} mode(s) reported:", display.modes.len());

            for mode in &display.modes {
                if mode.width == 0 || mode.height == 0 || mode.refresh_rate == 0 {
                    results.fail(
                        &format!("Vulkan:   {}x{}@{}mHz", mode.width, mode.height, mode.refresh_rate),
                        if mode.refresh_rate == 0 { "zero refresh rate" } else { "zero dimensions" },
                    );
                } else {
                    println!("       Vulkan:   {}x{}@{}mHz", mode.width, mode.height, mode.refresh_rate);
                }
            }

            // DCS should advertise exactly one mode per display (the
            // configured mode). Multiple modes or duplicates indicate a
            // bug in the WSI mode filtering.
            if display.modes.len() != 1 {
                results.fail(
                    &format!("Vulkan: {} display {} mode count", dev.device_name, i),
                    &format!("expected 1 mode, got {}", display.modes.len()),
                );
            } else {
                results.pass(&format!("Vulkan: {} display {} has exactly 1 mode", dev.device_name, i));
            }
        }
    }

    Some(phys_devices)
}

/// Cross-check: for each Vulkan display, verify that its modes include the
/// DCS-reported current mode.  This is a placeholder until DCS implements
/// `wl_output` globals and we can enumerate modes via the protocol.
fn run_crosscheck(
    results: &mut TestResults,
    _protocol: &dcs_protocol::DcsProtocolState,
    vk_devices: &[vulkan::VkPhysDeviceDisplays],
) {
    // DCS doesn't yet create wl_output globals, so we can't query output
    // modes via the DCS protocol (get_output requires a wl_output argument).
    // For now, just verify the Vulkan side is non-empty.
    let total_vk_modes: usize = vk_devices
        .iter()
        .flat_map(|d| &d.displays)
        .map(|d| d.modes.len())
        .sum();

    if total_vk_modes > 0 {
        results.pass(&format!(
            "Cross-check: Vulkan reports {} total mode(s)",
            total_vk_modes
        ));
    } else {
        results.fail("Cross-check: Vulkan reports modes", "no modes found");
    }

    // TODO: once DCS implements wl_output globals, enumerate modes via the
    // DCS protocol and verify each DCS mode appears in the Vulkan mode list
    // for the corresponding display.
    results.skip(
        "Cross-check: DCS protocol modes vs Vulkan modes",
        "DCS does not yet advertise wl_output globals",
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("vulkan-sample") {
        let opts = match vulkan_sample::parse_args(&args[1..]) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("error: {}", e);
                return ExitCode::from(2);
            }
        };
        return match vulkan_sample::run(&opts) {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("error: {}", e);
                ExitCode::from(1)
            }
        };
    }

    let mut protocol_only = false;

    for arg in &args {
        match arg.as_str() {
            "--protocol-only" => protocol_only = true,
            "--help" | "-h" => {
                eprintln!("Usage: dcs-test [OPTIONS]");
                eprintln!();
                eprintln!("Connects to the DCS Wayland socket (display-config-server-0)");
                eprintln!("or the socket specified by WAYLAND_DISPLAY.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --protocol-only   Skip Vulkan tests, only check DCS protocol");
                eprintln!("  --help            Show this help");
                eprintln!();
                eprintln!("Subcommands:");
                eprintln!("  vulkan-sample     Present a color cycle to all displays (Vulkan D2D)");
                eprintln!("    --duration N          Exit after N seconds (default: run until Ctrl+C)");
                eprintln!("    --present-barrier     Synchronize presents with VK_NV_present_barrier");
                return ExitCode::from(0);
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                return ExitCode::from(2);
            }
        }
    }

    let mut results = TestResults::new();

    // --- Protocol tests ---
    let protocol_state = run_protocol_tests(&mut results);

    // --- Vulkan tests ---
    if !protocol_only {
        let vk_devices = run_vulkan_tests(&mut results);

        if let (Some(proto), Some(vk)) = (&protocol_state, &vk_devices) {
            run_crosscheck(&mut results, proto, vk);
        }
    }

    results.summary()
}
