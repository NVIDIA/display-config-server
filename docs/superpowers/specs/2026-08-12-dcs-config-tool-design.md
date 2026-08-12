# dcs-config / dcs-tool Design

## Goal

Build `dcs-config` (a Rust client library) and `dcs-tool` (a CLI binary) that let users query the current display configuration from DCS and apply new configurations — from CLI flags, from a YAML file, or both. The library provides a reusable foundation for any future DCS client tool.

## Architecture

Two new crates added to the existing `display-config-server` Cargo workspace:

```
display-config-server/
  Cargo.toml               # workspace: adds dcs-config, dcs-tool
  dcs-config/              # library crate
    Cargo.toml
    src/
      lib.rs
      protocol.rs          # generated client bindings (moved from dcs-test)
      connection.rs        # connect to DCS socket, bind ZwpDcsManager
      output.rs            # enumerate wl_output globals, collect OutputInfo
      apply.rs             # commit a TopologyConfig via the Wayland protocol
      config.rs            # Config / TopologyConfig / DisplayConfig / ModeConfig structs
  dcs-tool/                # binary crate
    Cargo.toml
    src/
      main.rs              # CLI parsing (clap) → dcs_config calls
  dcs-test/
    src/
      dcs_protocol.rs      # replaces its own binding code with dcs_config
```

## Prerequisite: wl_output globals in DCS (separate commit)

`dcs-tool show` and `dcs-tool apply` both require `wl_output` globals to be advertised in the DCS Wayland registry (needed for `get_output`). The DCS server must be updated to create one Smithay `Output` object per connected display, tagging each with its `crtc::Handle` as `wl_output` resource user data so the existing `get_output` handler can resolve it. This is a self-contained server-side change committed independently, before the `dcs-config`/`dcs-tool` work.

## dcs-config Library

### Dependencies
- `wayland-client = "0.31"`
- `wayland-scanner = "0.31"`
- `anyhow = "1"`

### `config.rs` — Shared data types

```rust
pub struct ModeConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

pub struct DisplayConfig {
    pub number: u32,
    pub mode: Option<ModeConfig>,
}

pub struct TopologyConfig {
    pub display: Vec<DisplayConfig>,
}

pub struct Config {
    pub topology: Vec<TopologyConfig>,
}
```

These are the canonical types both the CLI path and YAML path deserialize into before calling `apply`. `serde` derives are on `Config`, `TopologyConfig`, `DisplayConfig`, and `ModeConfig` so the YAML crate can deserialize directly.

### `protocol.rs` — Client-side protocol bindings

Generates client code from the protocol XML (currently lives in `dcs-test`; moved here):

```rust
pub mod zwp_display_config_server_v1 {
    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::wl_output;
    use wayland_client::protocol::__interfaces::{wl_output_interface, WL_OUTPUT_INTERFACE};
    wayland_scanner::generate_interfaces!("../protocol/zwp_display_config_server_v1.xml");
    wayland_scanner::generate_client_code!("../protocol/zwp_display_config_server_v1.xml");
}
```

### `connection.rs` — Wayland connection

```rust
pub struct DcsConnection {
    pub conn: Connection,
    pub event_queue: EventQueue<DcsState>,
    pub manager: ZwpDcsManager,
}

pub fn connect() -> anyhow::Result<DcsConnection>
```

Connects to `display-config-server-0` (falls back to `$WAYLAND_DISPLAY`), does a registry roundtrip, and returns the bound `ZwpDcsManager`. Returns an error if DCS is not running.

### `output.rs` — Output enumeration

```rust
pub struct OutputInfo {
    pub display_number: i32,
    pub mode_width: u32,
    pub mode_height: u32,
    pub mode_refresh_mhz: u32,
    pub dev_t: u32,
}

pub fn enumerate_outputs(conn: &mut DcsConnection) -> anyhow::Result<Vec<OutputInfo>>
```

Binds each `wl_output` global, calls `manager.get_output`, collects `mode`/`device`/`number`/`done` events, and returns `Vec<OutputInfo>` once all outputs are done.

### `apply.rs` — Apply a topology

```rust
pub fn apply(conn: &mut DcsConnection, topology: &TopologyConfig) -> anyhow::Result<()>
```

For each `DisplayConfig` in `topology.display`:
1. Finds the matching `wl_output` by `display_number`
2. Calls `manager.get_output` → `dcs_output.create_configuration`
3. Calls `set_mode(width, height, refresh_mhz)` if a mode is specified
4. Calls `topology.add_configuration(config)`

Then calls `topology.commit()` and does a roundtrip waiting for either a clean return or an error event (`ZwpDcsTopology::Error` or `ZwpDcsDisplayConfiguration::Error`). On error, returns `Err` with the error code and a description.

## dcs-tool Binary

### Dependencies
- `dcs-config` (local)
- `clap = { version = "4", features = ["derive"] }`
- `serde = { version = "1", features = ["derive"] }`
- `serde_yaml = "0.9"`
- `anyhow = "1"`

### CLI Interface

```
dcs-tool <SUBCOMMAND>

Subcommands:
  show                          Print current DCS display configuration
  apply [OPTIONS]               Apply a display configuration

apply options:
  --config <FILE>               YAML config file
  --display <N>                 Display number (may be repeated with --mode)
  --mode <WxH@R>                Mode in WIDTHxHEIGHT@REFRESHmHz (paired with --display)
```

`--display` and `--mode` must appear in pairs. Multiple pairs are allowed for multi-display commits. `--config` and `--display`/`--mode` are mutually exclusive.

### `show` output

```
Display 1  (dev 226:0)  1920x1080@60000mHz  [current]
Display 2  (dev 226:0)  1920x1080@60000mHz  [current]
```

### `apply` flow

**YAML path:** Deserializes the file into `Config`, iterates `Config.topology`, calls `dcs_config::apply(conn, &topology)` for each.

**CLI path:** Collects `--display`/`--mode` pairs into a single `TopologyConfig`, calls `dcs_config::apply(conn, &topology)` once.

Both paths use the same `apply` function. Exit code 0 on success, 1 on failure with a message on stderr.

### YAML format

```yaml
topology:
  - display:
      - number: 1
        mode:
          width: 1920
          height: 1080
          refresh_mhz: 60000
      - number: 2
        mode:
          width: 1920
          height: 1080
          refresh_mhz: 60000
```

`topology` is a list for future extensibility (e.g. framelock requiring sequential topology commits). For the initial prototype a single topology is expected.

## dcs-test Migration

`dcs-test/src/dcs_protocol.rs` is replaced with a dependency on `dcs-config`. It uses `dcs_config::connection::connect()` and `dcs_config::output::enumerate_outputs()` instead of its own inline Wayland code.

## Error Handling

- DCS not running → clear message: `"Failed to connect to DCS: socket not found"`
- Unknown display number → `"Display N not found in DCS output list"`
- Topology commit rejected → protocol error code forwarded: `"Topology commit failed: InvalidState on display N"`
- YAML parse failure → `serde_yaml` error forwarded to stderr

## Out of Scope for Prototype

- QuadroSync sub-protocol
- `set_number` request
- Saving current config to YAML
- Config file at well-known path on startup
