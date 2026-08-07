// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! Trait abstractions for display configuration features.
//!
//! [`DisplayAttribute`] represents a per-output setting (mode, display number).
//! [`TopologyAttribute`] represents a cross-output setting (QuadroSync) that
//! owns child [`DisplayAttribute`]s and applies them as part of its own logic.
//!
//! Both are accumulated in a [`PendingCommit`] and applied atomically when
//! the config tool commits a topology.

pub mod display_number;
pub mod mode;
pub mod quadro_sync;

use drm::control::crtc;

use crate::render::dcs_output::DcsOutput;
use crate::render::DcsDevice;

/// A per-output attribute that configures one display.
///
/// Created from protocol display_configuration requests.
/// May be standalone (e.g., mode change) or owned by a [`TopologyAttribute`]
/// (e.g., QuadroSync role assignment).
pub trait DisplayAttribute: Send + Sync {
    /// Human-readable name for logging/debugging.
    fn name(&self) -> &str;
    /// Validate this attribute against the output's current state.
    /// Called before any attributes are applied.
    fn validate(&self, output: &DcsOutput) -> anyhow::Result<()>;
    /// Apply this attribute, mutating the output's state.
    fn apply(&self, output: &mut DcsOutput) -> anyhow::Result<()>;
}

/// A cross-output attribute that configures a feature spanning multiple displays.
///
/// Owns child [`DisplayAttribute`]s representing the per-output settings that
/// this topology attribute controls.  [`apply()`](TopologyAttribute::apply) is
/// the sole entry point — it internally applies its children as part of its
/// own logic.
pub trait TopologyAttribute: Send + Sync {
    /// Human-readable name for logging/debugging.
    fn name(&self) -> &str;
    /// Per-output attributes owned by this topology attribute.
    fn display_attributes(&self) -> &[(crtc::Handle, Box<dyn DisplayAttribute>)];
    /// Validate cross-output invariants (e.g., exactly one server).
    fn validate(&self, device: &DcsDevice) -> anyhow::Result<()>;
    /// Apply this attribute, including all owned child display attributes.
    fn apply(&self, device: &mut DcsDevice) -> anyhow::Result<()>;
}

/// Accumulated attribute changes, applied atomically on topology commit.
///
/// Lives in [`DcsState`](crate::DcsState) because a topology commit can
/// span multiple devices.
pub struct PendingCommit {
    /// Cross-output attributes (e.g., QuadroSync).  Each owns its child
    /// DisplayAttributes and applies them internally.
    pub topology_attrs: Vec<Box<dyn TopologyAttribute>>,

    /// Standalone per-output attributes not owned by any topology
    /// (e.g., mode change, display number).
    pub display_attrs: Vec<(crtc::Handle, Box<dyn DisplayAttribute>)>,
}

impl PendingCommit {
    pub fn new() -> Self {
        Self {
            topology_attrs: Vec::new(),
            display_attrs: Vec::new(),
        }
    }
}
