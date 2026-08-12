// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! `wl_output` global creation for DCS.
//!
//! DCS creates one `wl_output` global per connected display.  Each global
//! carries the output's `crtc::Handle` as resource user data so that the
//! `zwp_dcs_manager.get_output` handler can resolve it via
//! `output.data::<crtc::Handle>().copied()`.

use drm::control::crtc;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    protocol::wl_output::{self, Mode as WlOutputMode, Subpixel, Transform, WlOutput},
};

use crate::DcsState;

impl GlobalDispatch<WlOutput, crtc::Handle> for DcsState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlOutput>,
        global_data: &crtc::Handle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let crtc = *global_data;
        let output = data_init.init(resource, crtc);
        if let Some(dcs_out) = state.output_for_crtc(crtc) {
            output.geometry(
                0,
                0,
                0,
                0,
                Subpixel::Unknown,
                "NVIDIA".into(),
                format!("DCS-{}", dcs_out.display_number),
                Transform::Normal,
            );
            output.mode(
                WlOutputMode::Current,
                dcs_out.mode_width as i32,
                dcs_out.mode_height as i32,
                dcs_out.mode_refresh_mhz as i32,
            );
            output.scale(1);
            output.done();
        }
    }
}

impl Dispatch<WlOutput, crtc::Handle> for DcsState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlOutput,
        request: wl_output::Request,
        _data: &crtc::Handle,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_output::Request::Release => {
                // Destructor; wayland-server cleans up the resource automatically.
            }
            _ => {}
        }
    }
}
