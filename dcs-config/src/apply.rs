// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
use crate::{connection::DcsClient, config::TopologyConfig};

impl DcsClient {
    /// Apply a topology to DCS atomically.
    ///
    /// Builds a `zwp_dcs_topology`, adds one `zwp_dcs_display_configuration`
    /// per display in `topology.display`, and commits.  Returns `Err` if the
    /// server rejects the commit.
    pub fn apply(&mut self, topology: &TopologyConfig) -> anyhow::Result<()> {
        todo!("implemented in Task 5")
    }
}
