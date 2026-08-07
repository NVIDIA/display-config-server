// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Display number attribute.

use crate::render::dcs_output::DcsOutput;
use super::DisplayAttribute;

/// Changes the display's ID number (shown on the splash screen).
pub struct DisplayNumberAttribute {
    pub number: u32,
}

impl DisplayAttribute for DisplayNumberAttribute {
    fn name(&self) -> &str {
        "display_number"
    }

    fn validate(&self, _output: &DcsOutput) -> anyhow::Result<()> {
        // Display number changes always succeed.
        Ok(())
    }

    fn apply(&self, output: &mut DcsOutput) -> anyhow::Result<()> {
        output.display_number = self.number as i32;
        output.needs_render = true;
        Ok(())
    }
}
