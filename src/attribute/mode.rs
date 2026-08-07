// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Mode change display attribute.

use crate::render::dcs_output::DcsOutput;
use super::DisplayAttribute;

/// Changes the display mode (resolution and refresh rate).
pub struct ModeAttribute {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl DisplayAttribute for ModeAttribute {
    fn name(&self) -> &str {
        "mode"
    }

    fn validate(&self, output: &DcsOutput) -> anyhow::Result<()> {
        output.test_mode_change_by_params(self.width, self.height, self.refresh_mhz)
    }

    fn apply(&self, output: &mut DcsOutput) -> anyhow::Result<()> {
        output.apply_mode_change_by_params(self.width, self.height, self.refresh_mhz)
    }
}
