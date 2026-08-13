// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! DCS client library.
//!
//! Provides connection management, output enumeration, and topology apply
//! for the Display Config Server's private Wayland protocol.

pub mod config;
pub mod connection;
pub mod output;
pub mod apply;
pub(crate) mod protocol;

pub use config::{Config, DisplayConfig, ModeConfig, TopologyConfig};
pub use connection::DcsClient;
pub use output::{ModeInfo, OutputInfo};
