# DCS Architecture

This document explains how the Display Config Server (DCS) and its companion
crates work internally: what the major types are, where they live, how the
logic is organized, and the exact code paths for the two most important
end-to-end flows — committing a display configuration with `dcs-tool`, and a
Vulkan client leasing and driving displays through the NVIDIA driver.

## Crate map

| Crate | Kind | Purpose |
|---|---|---|
| `display-config-server` (root, `src/`) | binary | The server: a minimal Smithay-based Wayland compositor that owns the DRM device, renders a splash on idle displays, leases displays to clients via `wp_drm_lease_device_v1`, and applies configuration changes atomically via the private `zwp_display_config_server_v1` protocol (plus the `zwp_dcs_quadro_sync_v1` sub-protocol for framelock). |
| `dcs-client` | library | Wayland *client* library for the DCS private protocols: connect, enumerate outputs/modes/QuadroSync state, apply topology configurations. |
| `dcs-tool` | binary | CLI front-end over `dcs-client`: `show` (print current state) and `apply` (from CLI flags or a YAML file). |
| `dcs-test` | binary | Test client: verifies protocol globals and `VK_KHR_display` enumeration, and provides `vulkan-sample`, a Vulkan direct-to-display client that presents a synchronized color cycle to all displays. |

Protocol XML lives in `protocol/`:
`zwp_display_config_server_v1.xml` (core) and `zwp_dcs_quadro_sync_v1.xml`
(QuadroSync extension). Both the server (`src/protocols.rs`) and the client
library (`dcs-client/src/protocol.rs`) generate their bindings from the same
XML with `wayland-scanner` (`generate_server_code!` / `generate_client_code!`).

---

## The server

### Top-level state (`src/main.rs`)

- **`DcsState`** (`src/main.rs`) — the root compositor state passed to
  every Wayland dispatch handler.
  - `devices: Vec<DcsDevice>` — one per DRM device (GPU) DCS manages.
  - `pending_commit: Option<attribute::PendingCommit>` — staging area for an
    in-progress topology commit (see the attribute system below).
  - Helpers: `output_for_handle`/`output_for_handle_mut`/`device_for_handle` take an
    `OutputHandle` (device index + CRTC) and resolve it to the output or device;
    `validate_output_mode_change` and `commit_output_mode_change` forward to
    the owning device. `OutputHandle` (`src/render/dcs_output.rs`) exists because CRTC
    and connector handles are per-device DRM ids that two GPUs can reuse.
- **`DcsCalloopData`** (`src/main.rs`) — `{ state: DcsState, display: Display<DcsState> }`,
  the value threaded through the calloop event loop.

`main` (`src/main.rs`) does, in order:

1. `parse_card_selection` picks the cards: no argument opens every `/dev/dri/card*` device (`drm_card_paths`); `--card N` or a positional path
   opens just that one. One `DcsDevice` per card, display numbers assigned
   consecutively across devices, then one initial `render()` per device.
2. Creates the calloop `EventLoop` and inserts three sources:
   - one DRM notifier **per device**: `DrmEvent::VBlank(crtc)` → that device's
     `frame_submitted` (completes the page-flip cycle for that output);
   - the Wayland display poll fd (`src/main.rs`) → `dispatch_clients`;
   - a `ListeningSocketSource` named **`display-config-server-0`**
     (`src/main.rs`) → `insert_client`.
3. Registers the Wayland globals:
   - `ZwpDcsManager` v1 — always;
   - `ZwpDcsQuadroSyncManager` v1 — when **any** device has framelock
     hardware, so a client can feature-detect QuadroSync by the global's presence;
   - one `WlOutput` v4 per connected CRTC on every device, with an `OutputHandle`
     (device index + CRTC) as the global's user data — this is the client's
     handle for naming a display in protocol requests;
   - `DrmLeaseState::new` runs once per device, publishing the standard
     `wp_drm_lease_device_v1` global and advertising each device's connectors.
4. Runs the loop. The **post-dispatch callback ordering matters**
   (`src/main.rs`): `dispatch_clients` → per-device `render()` →
   `flush_clients`. Client requests processed in a batch (e.g. a topology
   commit that changed a mode) are therefore rendered — and their staged
   atomic state actually committed to KMS — in the *same* loop iteration.

### Devices and outputs (`src/render/`)

- **`DcsDevice`** (`src/render/dcs_device.rs:33`) — one DRM device/GPU.
  - `drm_device: DrmDevice`, `drm_node: DrmNode` — Smithay's DRM wrapper.
  - `renderer: GlesRenderer` — a single GBM→EGL→GLES renderer shared by all
    outputs on this GPU (built in `new`, `:83-88`).
  - `outputs: HashMap<crtc::Handle, DcsOutput>` — one per connected
    connector (enumerated in `new`, `:115-153`; filterable with the
    `DCS_CONNECTOR` env var, `:108`).
  - `drm_lease_state: Option<DrmLeaseState>` — the lessor side of DRM
    leasing.
  - `quadro_sync_state: Option<QuadroSyncState>` — present iff framelock
    hardware detected (`:167`).
  - Key methods: `render()` (`:193`, renders every output that needs it),
    `validate_output_mode_change` → `test_mode_change` (`:244`),
    `commit_output_mode_change` → `apply_mode_change` (`:261`),
    `frame_submitted` (`:274`).

- **`DcsOutput`** (`src/render/dcs_output.rs:41`) — one display.
  - `compositor: GbmDrmCompositor` — Smithay's `DrmCompositor` over a
    `DrmSurface`; owns the CRTC and performs atomic commits.
  - `splash: MemoryRenderBuffer` + `icon_rgba` — the idle image, rebuilt at
    the current mode's size (`build_splash`, `:78`).
  - `needs_render: bool` — the *only* render gate. `render_with` (`:261`)
    returns early when it's false; anything that should change what's on
    screen sets it.
  - `connector_handle`, `connector_modes` — the DRM connector and its full
    (raw, possibly duplicate-containing) mode list.
  - `active_lease: Option<DrmLease>` — set while a client holds this
    display's lease.
  - `display_number`, `mode_width/height/refresh_mhz`, `dev_t` — the state
    reported to clients over the protocol.
  - Key methods:
    - `render_with` (`:261`) — draws the splash element and calls
      `compositor.render_frame()` + `queue_frame()` (`:289`), then clears
      `needs_render`. `frame_submitted` (`:457`) completes the flip on
      VBlank.
    - `test_mode_change` (`:403`) — stages the mode with
      `compositor.use_mode()` and runs a KMS **TEST_ONLY** atomic commit,
      then reverts. This is what makes configuration validation
      non-destructive.
    - `apply_mode_change` (`:430`) — `use_mode()` for real, updates the
      cached mode fields, rebuilds the splash, and crucially calls
      `compositor.reset_state()` + sets `needs_render`. `reset_state()`
      forces full damage so the *next* `render_frame` cannot conclude
      "nothing changed" and skip the atomic commit that carries the
      modeset — without it, a mode change stays staged in the compositor
      until some unrelated event flushes it.
    - `request_redraw` (`:319`) — used when a lease ends: `reset_state()` +
      `needs_render = true`, so DCS repaints the splash without a full
      modeset.
    - `primary_plane_with_claim` (`:306`) — hands the primary plane to the
      lease builder.

### The attribute system (`src/attribute/`)

Configuration changes are modeled as *attributes* — small objects staged
during protocol dispatch and applied in one validated batch on commit.

- **`trait DisplayAttribute`** (`src/attribute/mod.rs:26`) — a per-display
  setting:
  ```rust
  fn name(&self) -> &str;
  fn validate(&self, output: &DcsOutput) -> anyhow::Result<()>;
  fn apply(&self, output: &mut DcsOutput) -> anyhow::Result<()>;
  ```
- **`trait TopologyAttribute`** (`src/attribute/mod.rs:41`) — a cross-display
  setting. Each attribute owns a single device; `display_attributes()` returns
  per-display settings keyed by `OutputHandle`, and `output_handles()` lets
  `apply_topology` find the one device owning the attribute, rejecting topologies
  that span GPUs (cross-GPU framelock is future work):
  ```rust
  fn name(&self) -> &str;
  fn display_attributes(&self) -> &[(OutputHandle, Box<dyn DisplayAttribute>)];
  fn output_handles(&self) -> Vec<OutputHandle>;
  fn validate(&self, device: &DcsDevice) -> anyhow::Result<()>;
  fn apply(&self, device: &mut DcsDevice) -> anyhow::Result<()>;
  ```
- **`PendingCommit`** (`src/attribute/mod.rs:59`) — the staging container:
  `topology_attrs: Vec<Box<dyn TopologyAttribute>>` and
  `display_attrs: Vec<(OutputHandle, Box<dyn DisplayAttribute>)>`. It lives
  in `DcsState.pending_commit` and is drained by `apply_topology`.

Implementations:

| Attribute | File | What it does |
|---|---|---|
| `ModeAttribute {width, height, refresh_mhz}` | `src/attribute/mode.rs` | Delegates to `DcsOutput::test_mode_change_by_params` / `apply_mode_change_by_params` — resolution/refresh change with TEST_ONLY validation. |
| `DisplayNumberAttribute {number}` | `src/attribute/display_number.rs` | Renumbers a display; apply updates `display_number` and re-renders. |
| `QuadroSyncRoleAttribute {connector_id, role}` | `src/attribute/quadro_sync.rs:180` | Per-display framelock role. Its `DisplayAttribute` validate/apply are intentionally no-ops — the parent topology attribute applies roles, because framelock is inherently cross-display. |
| `QuadroSyncTopologyAttribute` | `src/attribute/quadro_sync.rs:217` | The real framelock worker: `roles`, `sync_delay`, `polarity`, `house_sync_mode`, `sync_enable`, `framelock_index`. `validate` (`:240`) enforces exactly one server + ≥1 client when enabling sync. `apply` (`:273`) drives the hardware via custom nvidia-drm ioctls: disable sync → `SET_DISPLAY_CONFIG` per role → `SET_ATTRIBUTE` for delay/polarity/house-sync → re-enable sync on the server connector. |

QuadroSync hardware detection (`QuadroSyncState`, `detect_quadro_sync`,
`src/attribute/quadro_sync.rs:88/:95`) uses the `DRM_NVIDIA_FRAMELOCK_QUERY`
ioctl at device startup; its presence gates the QuadroSync Wayland global.

### The protocol layer (`src/protocol/`)

Wayland objects carry *user-data* structs that accumulate the client's staged
state; nothing touches hardware until `commit`.

Core protocol (`src/protocol/zwp_display_config_server_v1.rs`):

| User data | Attached to | Holds |
|---|---|---|
| `WlDcsOutput {handle}` | `zwp_dcs_output` | Which display (as an `OutputHandle`) this object refers to. |
| `Mutex<WlDcsDisplayConfiguration>` | `zwp_dcs_display_configuration` | `handle` (OutputHandle), `pending_mode: (w, h, refresh_mhz)`, `pending_number`, and `quadro_sync_config: Option<ZwpDcsQuadroSyncDisplayConfiguration>` — the link that lets commit find the QuadroSync extension of this config. |
| `Mutex<WlDcsTopology>` | `zwp_dcs_topology` | `configurations: Vec<ZwpDcsDisplayConfiguration>` and `quadro_sync_topology: Option<ZwpDcsQuadroSyncTopology>`. |

Request handlers:

- `GetOutput` — resolves the `OutputHandle` from the `wl_output` argument's user
  data, then emits the output's info events: one `mode` event per
  **deduplicated** `(w, h, refresh)` (raw DRM mode lists can contain the
  same visible mode twice — EDID detailed timing vs. CEA block), tagged
  `current`/`preferred`/`none`, plus `device` (dev_t), `number`, `done`.
- `CreateTopology` (`:174`), `CreateConfiguration` (`:202`) — create the
  staging objects.
- `SetMode`/`SetNumber` (`:221`) — record pending values in the config's
  user data.
- `AddConfiguration` (`:262`) — registers a config with the topology,
  rejecting a duplicate CRTC with `TopologyError::DuplicateConfigs`.
- `Commit` (`:292`) — calls `DcsState::apply_topology`; on failure sends
  `ConfigError::InvalidState` on the offending configuration and
  `TopologyError::Failed` on the topology, so the client learns both *that*
  it failed and *which display* was at fault.

**`apply_topology`** (`:323`) is the heart of a commit:

1. If the topology has a QuadroSync extension, collect each base config's
   `quadro_sync_config` and call `build_quadro_sync_attribute`, which resolves
   each config's `OutputHandle` to its connector id and pushes a
   `QuadroSyncTopologyAttribute` into `pending_commit`.
2. For each base configuration, push a `ModeAttribute` and/or
   `DisplayNumberAttribute` into `pending_commit` (`:352-378`).
3. Run four phases over the drained `PendingCommit` (`:380-408`):
   **validate all topology attrs → validate all display attrs → apply all
   topology attrs → apply all display attrs.** Validation is all-or-nothing
   and side-effect-free (TEST_ONLY commits, role-count checks), so a
   rejected commit leaves the hardware untouched.

QuadroSync sub-protocol (`src/protocol/zwp_dcs_quadro_sync_v1.rs`): each
QuadroSync object *extends* a base object passed as a request argument —
`get_output(zwp_dcs_output)`, `get_configuration(zwp_dcs_display_configuration)`,
`get_topology(zwp_dcs_topology)`. The handlers stash the extension object
into the base object's user data (`GetConfiguration` → base config's
`quadro_sync_config`, `:139-163`; `GetTopology` → base topology's
`quadro_sync_topology`, `:101-118`) so that the single base `commit` covers
everything. `SetRole`/`SetSyncDelay`/`SetPolarity`/`SetHouseSyncMode`/
`SetSyncEnable` just stage values. (`GetOutput` currently reports
placeholder `sync_status`/`role` events — live hardware readback is future
work.)

### DRM leasing (server side)

DCS is the **lessor**. Smithay's `DrmLeaseState` implements the
`wp_drm_lease_device_v1` protocol; DCS plugs in policy via
`impl DrmLeaseHandler for DcsState` (`src/main.rs`):

- `lease_request` (`:164`) — finds the device owning the requested
  connectors and builds a `DrmLeaseBuilder` containing the connector, its
  CRTC, and the primary plane (`DcsOutput::primary_plane_with_claim`).
- `new_active_lease` (`:199`) — stores the granted `DrmLease` in the
  output's `active_lease`. Nothing else is needed to stop DCS's rendering:
  `needs_render` is false after the last splash frame, so DCS simply never
  touches the CRTC while the lessee owns it.
- `lease_destroyed` (`:214`) — clears `active_lease` and calls
  `request_redraw()`, which does `compositor.reset_state()` +
  `needs_render = true`; the next loop iteration repaints the splash with a
  page flip (no modeset — the display never "blinks off").

---

## dcs-client (client library)

Everything a client needs to talk to DCS, in five modules:

- **`connection.rs`** — `connect()` (`connect_to_env`, defaulting
  `WAYLAND_DISPLAY` to `display-config-server-0`) does a registry roundtrip
  and returns a **`DcsClient`**:
  ```rust
  pub struct DcsClient {
      manager: ZwpDcsManager,          // bound core global
      state: ClientState,              // all dispatch-mutable state
      event_queue: EventQueue<ClientState>,
      qh: QueueHandle<ClientState>,
  }
  ```
  `ClientState` collects registry results (`wl_outputs: Vec<WlOutput>`,
  `quadro_sync_manager: Option<ZwpDcsQuadroSyncManager>`,
  `drm_lease_found: bool`), the per-enumeration scratch
  (`pending: Vec<PendingOutput>`), the cached `bound_outputs:
  Vec<BoundOutput>` (a `wl_output` proxy paired with its `OutputInfo`, so
  `apply` can look proxies up by display number), and the commit error flags
  (`topology_error`, `config_error`) set by the `Error` event dispatchers.
  All protocol events land in `Dispatch` impls here; notably
  `Dispatch<ZwpDcsOutput, usize>` uses the user-data index to route
  `mode`/`device`/`number`/`done` events into the right `pending` slot, and
  the same pattern (`Dispatch<ZwpDcsQuadroSyncOutput, usize>`) fills
  `qs_role`/`qs_sync_active`.
- **`output.rs`** — public data model + enumeration:
  ```rust
  pub struct ModeInfo { width, height, refresh_mhz, current, preferred }
  pub struct QuadroSyncOutputInfo { role: QuadroSyncRole, sync_active: bool }
  pub struct OutputInfo { display_number, dev_t, modes: Vec<ModeInfo>,
                          quadro_sync: Option<QuadroSyncOutputInfo> }
  ```
  `DcsClient::enumerate_outputs()` calls `manager.get_output(wl_output)`
  (and `quadro_sync_manager.get_output(dcs_output)` when bound) for every
  registry `wl_output`, roundtrips once, and builds `OutputInfo`s.
- **`config.rs`** — the serde types shared by the CLI and YAML paths:
  ```rust
  pub struct ModeConfig { width, height, refresh_mhz }
  pub struct DisplayConfig { number, mode: Option<ModeConfig>,
                             quadro_sync_role: Option<QuadroSyncRole> }
  pub struct QuadroSyncConfig { sync_delay, polarity, house_sync_mode, sync_enable } // all Option
  pub struct TopologyConfig { display: Vec<DisplayConfig>,
                              quadro_sync: Option<QuadroSyncConfig> }
  pub struct Config { topology: Vec<TopologyConfig> }   // top-level YAML
  ```
  YAML keys are singular (`topology:`, `display:`). Note: when any
  QuadroSync setting is applied with `sync_enable` unset, the server treats
  it as *disabled* (it does `unwrap_or(false)`), not "unchanged".
- **`apply.rs`** — `DcsClient::apply(&TopologyConfig)`; see Flow 1 below.
- **`protocol.rs`** — the generated client bindings for both protocol XMLs
  (the QuadroSync module re-exports the base module's interfaces so its
  generated code can reference them).

## dcs-tool

`dcs-tool/src/main.rs` is a clap CLI with two subcommands:

- `show` — `connect()` → `quadro_sync_supported()` → `enumerate_outputs()`,
  prints each display's full mode list (`* ...[current]`, `[preferred]`),
  its QuadroSync role/sync line when hardware is present, and a trailing
  `QuadroSync: supported | not detected`.
- `apply` — either `--config file.yaml` (parses `Config`, applies each
  `TopologyConfig` in order) or flags: repeated `--display N --mode WxH@R`
  pairs plus QuadroSync flags (`--qs-role N=disabled|server|client`,
  `--qs-sync-delay`, `--qs-polarity`, `--qs-house-sync`,
  `--qs-enable`/`--qs-disable`). The CLI path assembles exactly one
  `TopologyConfig`; a `--qs-role` for a display not named by `--display`
  produces a role-only `DisplayConfig { mode: None, .. }`, and
  board-settings-only invocations (just `--qs-enable`) are valid.

## dcs-test

`dcs-test/src/main.rs` is a pass/fail test harness (`TestResults`):
protocol checks (`dcs_protocol.rs`, which delegates connection to
`dcs_client::connection::connect`), `VK_KHR_display` enumeration checks
(`vulkan.rs::query_vk_displays` — for DCS-leased displays the driver must
advertise **exactly one mode** per display, the configured one), and a
cross-check between the two.

The `vulkan-sample` subcommand (`vulkan_sample.rs`) is a real Vulkan D2D
client: for every display on every physical device it creates a
`VkDisplayKHR` plane surface, a clear-only FIFO swapchain
(`TRANSFER_DST`, no render pass/pipeline), and presents an HSV color sweep
(10 s period) until Ctrl+C or `--duration N`. With `--present-barrier` it
enables `VK_NV_present_barrier` (device feature + per-surface support
checked up front via `VkSurfaceCapabilitiesPresentBarrierNV`, failing fast
if anything is missing) and chains `VkSwapchainPresentBarrierCreateInfoNV`
into every swapchain — with QuadroSync configured by `dcs-tool`, all
displays change color in lockstep. It contains **no Wayland lease code**:
the driver leases from DCS transparently (see Flow 2). Error paths tear
down partial Vulkan state (`PartialDevice`), and a second Ctrl+C
force-kills the process if a broken sync config stalls presentation.

---

## Flow 1: committing a configuration via dcs-tool

Example: `dcs-tool apply --display 1 --mode 1920x1080@60000 --qs-role 1=server --qs-role 2=client --qs-enable`

**Client side (dcs-tool → dcs-client):**

1. `cmd_apply_cli` (`dcs-tool/src/main.rs`) parses the flags into one
   `TopologyConfig` (mode for display 1; roles for 1 and 2 — display 2
   becomes a role-only entry; `QuadroSyncConfig { sync_enable: Some(true), .. }`).
2. `connect()` (`dcs-client/src/connection.rs`) opens the
   `display-config-server-0` socket, roundtrips the registry, binds
   `zwp_dcs_manager`, all `wl_output`s, and (if advertised)
   `zwp_dcs_quadro_sync_manager`.
3. `DcsClient::apply` (`dcs-client/src/apply.rs`):
   1. `enumerate_outputs()` if needed, so display numbers can be resolved
      to `wl_output` proxies (`bound_outputs`).
   2. Errors out immediately if the config contains QuadroSync settings but
      the QuadroSync global wasn't advertised (no hardware).
   3. `manager.create_topology()` → `wl_topology`.
   4. If any QuadroSync setting is present:
      `qs_manager.get_topology(&wl_topology)` (this link is what lets the
      server's commit find the framelock state), then `set_sync_delay` /
      `set_polarity` / `set_house_sync_mode` / `set_sync_enable` for
      whichever board settings were given.
   5. Per display: `manager.get_output(&wl_output)` → `dcs_out`;
      `dcs_out.create_configuration()` → `cfg`; `cfg.set_mode(w, h, mhz)`
      if a mode was requested; `wl_topology.add_configuration(&cfg)`; and
      for a role, `qs_manager.get_output(&dcs_out)` →
      `.get_configuration(&cfg)` → `.set_role(role)`.
   6. Reset the error flags, `wl_topology.commit()`, roundtrip, and fail if
      `topology_error`/`config_error` was set by an `Error` event.

**Server side:**

7. The socket fd wakes calloop; `dispatch_clients` runs every staged request
   handler: user-data structs accumulate `pending_mode`, `pending_role`,
   board settings, and the QuadroSync↔base links
   (`WlDcsDisplayConfiguration.quadro_sync_config`,
   `WlDcsTopology.quadro_sync_topology`).
8. The `Commit` handler (`src/protocol/zwp_display_config_server_v1.rs:292`)
   calls `apply_topology` (`:323`):
   - `build_quadro_sync_attribute` turns the staged QuadroSync state into a
     `QuadroSyncTopologyAttribute` in `pending_commit` (roles resolved
     OutputHandle → connector id);
   - each base config contributes a `ModeAttribute` /
     `DisplayNumberAttribute`;
   - **validate topology attrs** (framelock: exactly one server, ≥1 client)
     → **validate display attrs** (mode: KMS TEST_ONLY atomic commit) →
     **apply topology attrs** (framelock ioctls: disable sync, set roles,
     set board attributes, re-enable) → **apply display attrs**
     (`apply_mode_change`: `use_mode` + `reset_state` + `needs_render`).
   - Any validation failure aborts before anything is applied and the
     client gets `ConfigError::InvalidState` + `TopologyError::Failed`.
9. Still in the same loop iteration (post-dispatch callback,
   `src/main.rs`), `device.render()` runs: `reset_state()` forced full
   damage, so `render_frame` produces a frame and `queue_frame` performs
   the **atomic KMS commit that carries the modeset**. The VBlank event
   then delivers `frame_submitted`, completing the flip.
10. `flush_clients` sends any error events; the client's roundtrip in step 6
    returns and `dcs-tool` prints success or failure.

## Flow 2: a Vulkan ICD leasing and driving displays

This is the path a customer app (vkcube, `dcs-test vulkan-sample`, a display
wall application) takes. The app itself only uses standard `VK_KHR_display`;
the DCS integration lives in the NVIDIA driver's Vulkan WSI (bfm:
`drivers/OpenGL/vulkan/wsi/vkwsi_modeset_nvkms.cpp`, using ICD entry points
`vkIcdDcsLeaseCreate/Acquire/GetFd/...` from `vulkan_icd.h`).

1. **Enumeration.** The app enumerates displays/modes with `VK_KHR_display`.
   For DCS-managed displays the driver reports exactly one mode per display:
   the mode DCS currently has configured (the WSI queries it via
   `DcsGetConnectorMode`, backed by the DCS lease connection's
   `zwp_dcs_output` mode events).
2. **Surface + swapchain.** The app creates a `VkDisplayKHR` plane surface
   and a swapchain. During swapchain/display setup the WSI calls
   `DcsAcquireLease` (`vkwsi_modeset_nvkms.cpp:3144`):
   - it resolves the display to a DRM connector id,
   - `vkDcsLeaseAcquire(lease, minor, connectorId)` — the driver's DCS
     client connects to the DCS Wayland socket and drives
     `wp_drm_lease_device_v1`: `create_lease_request` → `request_connector`
     → `submit`;
   - **on the DCS side** this hits `DrmLeaseHandler::lease_request`
     (`src/main.rs`), which grants a lease containing the connector,
     CRTC, and primary plane; `new_active_lease` records it in
     `DcsOutput.active_lease`. DCS stops touching that CRTC (its render
     gate `needs_render` stays false).
   - `vkDcsLeaseGetFd` returns the leased DRM master fd, which the WSI
     wraps in a `VK_ACQUIRE_DISPLAY_TYPE_DRM` acquire — from here on the
     driver drives the display through the leased fd exactly as if it owned
     the device.
3. **Modeset avoidance.** `DcsHandleModeset` (`:3184`) compares the app's
   requested mode against the DCS-configured timings; when they match it
   skips the `SetMode` (just `AssignHeadToDpy`), avoiding a redundant
   modeset/link retrain. Otherwise it sets the mode on the leased fd.
4. **Presentation.** The app renders and presents; flips go directly through
   the leased DRM fd/NVKMS. Each `Flip` checks `IsDcsLeaseRevoked`
   (`:3131`); if DCS revoked the lease the swapchain returns
   `VK_ERROR_SURFACE_LOST_KHR` and the app must recreate.
5. **Present barrier / QuadroSync.** If the app enabled
   `VK_NV_present_barrier` (as `vulkan-sample --present-barrier` does), the
   WSI groups swapchains into a swap group and, once all joined swapchains
   are ready, calls `ActivatePresentBarrier` (`:2964`). Because a DCS lease
   is active, the driver **skips its own registry-key framelock
   configuration** (`:3026`) and only verifies sync is enabled — the roles
   and board settings are the ones DCS programmed at `dcs-tool apply` time
   (Flow 1's framelock ioctls). This is the seam that makes DCS the single
   owner of QuadroSync policy.
6. **Teardown.** The app exits / destroys the swapchain; the lease fd
   closes, the Wayland lease object dies, and Smithay calls
   `lease_destroyed` (`src/main.rs`): DCS clears `active_lease`, does
   `request_redraw()` (`reset_state` + `needs_render`), and the next loop
   iteration repaints the splash with a plain page flip — the display stays
   lit throughout.
