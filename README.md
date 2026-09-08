# Display Config Server

A leasing server that configures displays and leases them to Vulkan
Direct-to-Display (D2D) applications on demand. This supports display walls and
advanced workstation features on platforms which do not have X11-based features
such as NVIDIA Mosaic.

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
display-config-server [--card N | /dev/dri/cardN]
```

With no arguments, DCS opens every DRM card under /dev/dri and manages
all of their connected displays. Pass --card N (or a device path) to restrict
DCS to a single GPU.

Log output is controlled by the `RUST_LOG` environment variable (e.g.
`RUST_LOG=debug`).

## License

See [LICENSE](LICENSE).
