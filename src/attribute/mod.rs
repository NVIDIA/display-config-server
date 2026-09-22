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

use crate::protocol::zwp_display_config_server_v1::WlDcsTopology;
use crate::render::dcs_output::OutputHandle;
use crate::render::dcs_output::DcsOutput;
use crate::DcsState;


/// A per-output attribute that configures one display.
///
/// Created from the per-display requests on a protocol topology object.
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

/// A cross-output attribute that configures a feature that may span every
/// device DCS manages.
///
/// Owns child [`DisplayAttribute`]s representing the per-output settings that
/// this topology attribute controls.  [`apply()`](TopologyAttribute::apply) is
/// the sole entry point — it internally applies its children as part of its
/// own logic.
pub trait TopologyAttribute: Send + Sync {
    /// Human-readable name for logging/debugging.
    fn name(&self) -> &str;
    /// Per-output attributes owned by this topology attribute.
    fn display_attributes(&self) -> &[(OutputHandle, Box<dyn DisplayAttribute>)];
    /// Every output this topology attribute touches, possibly across devices.
    fn output_handles(&self) -> Vec<OutputHandle>;
    /// Validate cross-output invariants (e.g., exactly one server) against
    /// every device DCS manages. `topology` is the whole staged commit, so
    /// the attribute can take into account state other parts of the same
    /// commit will set (for example a mode change on a framelock member).
    ///
    /// The attribute owns the sub-protocol topology object it was built
    /// from and sends its own error events on it as problems are found,
    /// checking everything rather than stopping at the first. The return
    /// value says whether the attribute can be applied; `Err` carries the
    /// reasons for the log.
    fn validate(&self, state: &DcsState, topology: &WlDcsTopology) -> anyhow::Result<()>;
    /// Apply this attribute across devices, including all owned child display
    /// attributes.
    fn apply(&self, state: &mut DcsState) -> anyhow::Result<()>;
}

/// Accumulated attribute changes, applied atomically on topology commit.
///
/// Built and consumed entirely within the topology `Commit` handler; nothing
/// outside that handler holds one.
pub struct PendingCommit {
    /// Cross-output attributes (e.g., QuadroSync).  Each owns its child
    /// DisplayAttributes and applies them internally.
    pub topology_attrs: Vec<Box<dyn TopologyAttribute>>,

    /// Standalone per-output attributes not owned by any topology
    /// (e.g., mode change, display number).
    pub display_attrs: Vec<(OutputHandle, Box<dyn DisplayAttribute>)>,
}

impl PendingCommit {
    pub fn new() -> Self {
        Self {
            topology_attrs: Vec::new(),
            display_attrs: Vec::new(),
        }
    }
}
