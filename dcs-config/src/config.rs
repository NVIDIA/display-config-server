// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Shared configuration types used by both the CLI path and YAML deserialization.
//!
//! Both `dcs-tool apply --display N --mode WxH@R` and
//! `dcs-tool apply --config file.yaml` deserialize into these types before
//! calling [`crate::DcsClient::apply`].

use serde::{Deserialize, Serialize};

/// A display mode (resolution + refresh rate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeConfig {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (e.g. 60000 for 60 Hz), matching the
    /// DCS Wayland protocol's `refresh` argument.
    pub refresh_mhz: u32,
}

/// Desired configuration for one display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// DCS display number (1-based, as reported by the `number` event).
    pub number: u32,
    /// Mode to set.  `None` leaves the mode unchanged.
    pub mode: Option<ModeConfig>,
}

/// A set of display configurations committed as one atomic topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyConfig {
    pub display: Vec<DisplayConfig>,
}

/// Top-level YAML config.  Contains one or more topology commits applied
/// in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub topology: Vec<TopologyConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builds() {
        let c = Config {
            topology: vec![TopologyConfig {
                display: vec![DisplayConfig {
                    number: 1,
                    mode: Some(ModeConfig { width: 1920, height: 1080, refresh_mhz: 60000 }),
                }],
            }],
        };
        assert_eq!(c.topology.len(), 1);
        assert_eq!(c.topology[0].display[0].number, 1);
    }
}
