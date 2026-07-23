//! Client-side bindings for the DCS private Wayland protocol, and a helper
//! that connects to DCS and collects the advertised output state.

use std::sync::{Arc, Mutex};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};

/// Client-side generated code for the DCS private protocol.
pub mod protocol {
    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};

    wayland_scanner::generate_interfaces!("../protocol/zwp_display_config_server_v1.xml");
    wayland_scanner::generate_client_code!("../protocol/zwp_display_config_server_v1.xml");
}

/// A mode reported by a DCS output.
#[allow(dead_code)] // fields used once wl_output globals are implemented
#[derive(Debug, Clone)]
pub struct DcsMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub current: bool,
}

/// State collected for one DCS output.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DcsOutputInfo {
    pub modes: Vec<DcsMode>,
    pub dev_t: Option<u32>,
    pub display_number: Option<i32>,
    pub done: bool,
}

/// All state collected from a DCS Wayland connection.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DcsProtocolState {
    /// Whether the zwp_dcs_manager global was found in the registry.
    pub manager_found: bool,
    /// Whether wp_drm_lease_device_v1 was found.
    pub drm_lease_found: bool,
    /// Outputs collected via get_output (requires wl_output globals).
    pub outputs: Vec<DcsOutputInfo>,
}

/// Internal mutable state used during the Wayland roundtrip.
#[allow(dead_code)]
struct ClientState {
    result: Arc<Mutex<DcsProtocolState>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Registry events are handled by GlobalListContents automatically.
    }
}

/// Connect to the DCS Wayland socket and query the advertised globals.
///
/// Returns `DcsProtocolState` with information about what globals are
/// available. When DCS adds `wl_output` globals, this function will also
/// enumerate output modes via the `zwp_dcs_manager::get_output` path.
pub fn query_dcs(socket_name: Option<&str>) -> Result<DcsProtocolState, Box<dyn std::error::Error>> {
    let conn = if let Some(name) = socket_name {
        // Set WAYLAND_DISPLAY so the connection uses the right socket.
        std::env::set_var("WAYLAND_DISPLAY", name);
        Connection::connect_to_env()?
    } else {
        Connection::connect_to_env()?
    };

    let result = Arc::new(Mutex::new(DcsProtocolState {
        manager_found: false,
        drm_lease_found: false,
        outputs: Vec::new(),
    }));

    let mut state = ClientState {
        result: result.clone(),
    };

    let (globals, mut event_queue) = registry_queue_init::<ClientState>(&conn)?;

    // Do one roundtrip to collect globals.
    event_queue.roundtrip(&mut state)?;

    // Check which globals are advertised.
    {
        let mut r = result.lock().unwrap();
        for global in globals.contents().clone_list() {
            match global.interface.as_str() {
                "zwp_dcs_manager" => r.manager_found = true,
                "wp_drm_lease_device_v1" => r.drm_lease_found = true,
                _ => {}
            }
        }
    }

    Ok(Arc::try_unwrap(result).unwrap().into_inner().unwrap())
}
