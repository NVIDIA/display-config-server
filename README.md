# Display Config Server

A leasing server that configures displays and leases them to Vulkan
Direct-to-Display (D2D) applications on demand. This supports display walls and
advanced workstation features on platforms which do not have X11-based features
such as NVIDIA Mosaic.

> **Warning:** This project is under active development and is not ready for
> production use. The protocols, command line tools, and configuration format
> are unstable and may change without notice, and driver support is still under
> development. For now this project is only intended for experimental or
> reference use.

## Background

Historically, "display walls" made out of large numbers of displays deployed
in customer setups were configured using the NVIDIA X driver's SLI Mosaic
feature. This provided a desktop spanning the entire display wall, but also
inserted Xorg into the display stack and removed fine-grained control over
the displays from the application. As distros have transitioned to Wayland
this old approach is no longer available, and in many cases isn't desirable.
Display wall applications are recommended to move to Vulkan Direct to Display
(VK_KHR_display) instead of running as a client on a Mosaic desktop. This
allows the application to directly control all of the displays in the system
and avoids the overhead of keeping the X server in the loop.

This approach has many benefits, but this project aims to solve a few of the
downsides:
- Every application must initialize the display from scratch, which can be slow
  and disrupts advanced display features that depend on a stable,
  continuously-trained link — such as hardware framelock across a display wall.
- The application must control advanced display features themselves,
  and handle all aspects of setup and configuration.
- Some advanced display features aren't exposed through Vulkan, so applications
  currently cannot use them.

Display Config Server (DCS) fills this role: it opens the DRM device at
startup, trains the display link, and holds it open. It configures any advanced
display features so Vulkan D2D applications do not have to. The Vulkan
driver interacts with the Display Config Server to inherit the configuration
seamlessly during Vulkan D2D startup.

## System overview

Instead of using MOSAIC or other NVIDIA X11 driver specific features to
configure display walls, DCS will own displays and make them available to
clients. Under this new architecture there are three components involved in
driving a display wall:

```
┌───────────────────────┐                       ┌──────────────────────┐
│  Display Config Server│◄─────────────────────►│  Configuration Tool  │
│  (this binary)        │    config protocol    │                      │
└──────────┬────────────┘                       └──────────────────────┘
           │
           │  leasing protocol
           ▼
┌───────────────────────┐
│  Vulkan D2D App       │
│  (VK_KHR_display)     │
└───────────────────────┘
```

**Display Config Server** opens the DRM device, performs the initial modeset,
and renders a splash screen while no lease is active. As DCS fills the role
of a display server the design is much like a Wayland compositor and is
conceptually similar to gamescope. It exposes two protocols:

- `wp_drm_lease_device_v1` — the standard upstream protocol that transfers
  DRM file descriptor ownership to a Vulkan D2D application. The display link
  is already trained; the application drives it without issuing a modeset of
  its own.

- `zwp_display_config_server_v1` — a private protocol used by the configuration
  tool to query and modify display settings atomically.

**Configuration Tool** — A CLI that reads a saved configuration from disk and
forwards it to the server over the private protocol. Users also invoke it
directly to change settings at runtime without restarting the server.

**Vulkan D2D Application** — connects to the DCS Wayland socket, discovers the
pre-configured display via `wp_drm_lease_device_v1`, and presents frames
directly without a modeset. Because the display link was established by DCS, the
link training state is preserved across lease transitions, which is a prerequisite
for hardware framelock on multi-display walls.

## Design

DCS uses [Smithay](https://github.com/Smithay/smithay) as its compositor
toolkit.

### Multi-output and multi-GPU

Each GPU is represented by a `DcsDevice` which holds the shared renderer and
a `HashMap<crtc::Handle, DcsOutput>` of its outputs. Multiple `DcsDevice`
instances can coexist in `DcsState::devices`. CRTC handles are the primary key
used throughout the codebase for O(1) VBlank dispatch and for linking Wayland
protocol objects back to their backing display state.

### Splash screen

While no lease is active each output renders a simple splash screen — a black
background tiled with a small icon. The splash is generated as a CPU-side RGBA
buffer (`MemoryRenderBuffer`) and uploaded to the GPU on the first render. It is
rebuilt at the new resolution whenever a mode change is applied.

## Building

```
cargo build --release
```

The binary links against the system's DRM, GBM, and EGL libraries. Ensure the
relevant development headers are installed for your distribution.

## Running

```
display-config-server [--card N | /dev/dri/cardN] [--connector ID]
```

With no arguments, DCS opens every DRM card under /dev/dri and manages
all of their connected displays. Pass --card N (or a device path) to restrict
DCS to a single GPU. Pass --connector ID to initialise only the connector
with that DRM object id, which is useful when debugging a single display.

Log output is controlled by the `RUST_LOG` environment variable (e.g.
`RUST_LOG=debug`).

## Usage examples

The examples below assume `display-config-server` and `dcs-tool` are on your
`PATH` and that you are running as a user with access to `/dev/dri`.

### Starting DCS

Start DCS managing all connected displays across every GPU:

```sh
display-config-server
```

To restrict DCS to one GPU and enable verbose logging:

```sh
RUST_LOG=debug display-config-server --card 0
```

DCS holds the display link open and renders a splash screen until a Vulkan D2D
application connects. It listens on a Wayland socket; clients discover it via
the standard `WAYLAND_DISPLAY` environment variable.

### Showing the current configuration

```sh
dcs-tool show
```

Example output with two displays and RTX Pro Sync hardware present:

```
Display  1  (dev 234881027)
  * 3840x2160@60000mHz [current]
    3840x2160@30000mHz
    1920x1080@60000mHz [preferred]
  QuadroSync role: disabled, sync: inactive, board: 0

Display  2  (dev 234881027)
  * 1920x1080@60000mHz [current] [preferred]
  QuadroSync role: disabled, sync: inactive, board: 0

QuadroSync: supported
```

The asterisk (`*`) marks the active mode. Display numbers are assigned by DCS
and are used as the target for all `apply` commands.

### Applying a mode to a single display

Set display 1 to 1920×1080 at 60 Hz:

```sh
dcs-tool apply --display 1 --mode 1920x1080@60000
```

The refresh rate is in millihertz. 60 Hz = `60000`, 120 Hz = `120000`.

### Configuring multiple displays

`--display` and `--mode` are paired positionally and may be repeated to
configure several displays in one atomic commit:

```sh
dcs-tool apply \
  --display 1 --mode 3840x2160@60000 \
  --display 2 --mode 3840x2160@60000
```

Alternatively, write the configuration to a YAML file and apply it:

```yaml
# /etc/dcs/wall.yaml
topology:
  - display:
      - number: 1
        mode: {width: 3840, height: 2160, refresh_mhz: 60000}
      - number: 2
        mode: {width: 3840, height: 2160, refresh_mhz: 60000}
```

```sh
dcs-tool apply --config /etc/dcs/wall.yaml
```

### RTX Pro Sync across two systems

RTX Pro Sync (formerly QuadroSync) locks the scan-out timing of displays on
separate machines to a common sync signal via a hardware cable between their
RTX Pro Sync boards. One machine acts as the framelock **server** and the
other as a framelock **client**. Both displays must run at the same refresh
rate.

**Machine A — framelock server**

```sh
dcs-tool apply \
  --display 1 --mode 1920x1080@60000 \
  --qs-role 1=server \
  --qs-enable
```

**Machine B — framelock client**

```sh
dcs-tool apply \
  --display 1 --mode 1920x1080@60000 \
  --qs-role 1=client \
  --qs-enable
```

The same setup expressed as YAML config files (useful for applying the
configuration at boot via a service unit):

```yaml
# Machine A — /etc/dcs/sync-server.yaml
topology:
  - display:
      - number: 1
        mode: {width: 1920, height: 1080, refresh_mhz: 60000}
        quadro_sync_role: server
    quadro_sync:
      sync_enable: true
```

```yaml
# Machine B — /etc/dcs/sync-client.yaml
topology:
  - display:
      - number: 1
        mode: {width: 1920, height: 1080, refresh_mhz: 60000}
        quadro_sync_role: client
    quadro_sync:
      sync_enable: true
```

```sh
dcs-tool apply --config /etc/dcs/sync-server.yaml   # on machine A
dcs-tool apply --config /etc/dcs/sync-client.yaml   # on machine B
```

Optional board-level settings can be added to the server topology when needed:

```yaml
    quadro_sync:
      sync_delay: 0
      polarity: rising_edge       # rising_edge | falling_edge | both_edges
      house_sync_mode: disabled   # disabled | input | output
      sync_enable: true
```

After applying, run `dcs-tool show` on each machine to confirm
`QuadroSync role: server` / `client` and `sync: active`.

## License

See [LICENSE](LICENSE).
