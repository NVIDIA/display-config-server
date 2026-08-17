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

/// QuadroSync (framelock) role for one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuadroSyncRole {
    Disabled,
    Server,
    Client,
}

/// QuadroSync sync signal polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuadroSyncPolarity {
    RisingEdge,
    FallingEdge,
    BothEdges,
}

/// House sync (external sync source) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HouseSyncMode {
    Disabled,
    Input,
    Output,
}

/// Board-level QuadroSync settings for one topology commit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuadroSyncConfig {
    pub sync_delay: Option<u32>,
    pub polarity: Option<QuadroSyncPolarity>,
    pub house_sync_mode: Option<HouseSyncMode>,
    pub sync_enable: Option<bool>,
}

/// Desired configuration for one display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// DCS display number (1-based, as reported by the `number` event).
    pub number: u32,
    /// Mode to set.  `None` leaves the mode unchanged.
    pub mode: Option<ModeConfig>,
    /// QuadroSync role for this display.  `None` leaves the role unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quadro_sync_role: Option<QuadroSyncRole>,
}

/// A set of display configurations committed as one atomic topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyConfig {
    pub display: Vec<DisplayConfig>,
    /// Board-level QuadroSync settings applied with this topology.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quadro_sync: Option<QuadroSyncConfig>,
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
                    quadro_sync_role: None,
                }],
                quadro_sync: None,
            }],
        };
        assert_eq!(c.topology.len(), 1);
        assert_eq!(c.topology[0].display[0].number, 1);
    }

    #[test]
    fn yaml_with_quadro_sync_parses() {
        let yaml = r#"
topology:
  - display:
      - number: 1
        mode: {width: 1920, height: 1080, refresh_mhz: 60000}
        quadro_sync_role: server
      - number: 2
        quadro_sync_role: client
    quadro_sync:
      sync_delay: 0
      polarity: rising_edge
      house_sync_mode: disabled
      sync_enable: true
"#;
        let c: Config = serde_yaml::from_str(yaml).unwrap();
        let topo = &c.topology[0];
        assert_eq!(topo.display[0].quadro_sync_role, Some(QuadroSyncRole::Server));
        assert_eq!(topo.display[1].quadro_sync_role, Some(QuadroSyncRole::Client));
        assert!(topo.display[1].mode.is_none());
        let qs = topo.quadro_sync.as_ref().unwrap();
        assert_eq!(qs.sync_delay, Some(0));
        assert_eq!(qs.polarity, Some(QuadroSyncPolarity::RisingEdge));
        assert_eq!(qs.house_sync_mode, Some(HouseSyncMode::Disabled));
        assert_eq!(qs.sync_enable, Some(true));
    }

    #[test]
    fn yaml_without_quadro_sync_still_parses() {
        let yaml = r#"
topology:
  - display:
      - number: 1
        mode: {width: 1920, height: 1080, refresh_mhz: 60000}
"#;
        let c: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(c.topology[0].display[0].quadro_sync_role.is_none());
        assert!(c.topology[0].quadro_sync.is_none());
    }
}
