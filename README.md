# sway-mirror

Fast zero-copy screen mirroring for Sway/wlroots compositors.

## Features

- **Zero-copy rendering** - Uses DMA-BUF for efficient GPU-to-GPU frame transfer, including tiled/compressed buffers (DRM format modifiers)
- **Works on any GPU** - Automatically falls back to a CPU copy when zero-copy isn't possible (e.g. hybrid laptops, software rendering)
- **Multiple capture protocols** - `wlr-export-dmabuf`, `ext-image-copy-capture` and `wlr-screencopy`
- **Multi-output support** - Mirror to one or all other outputs simultaneously
- **Workspace management** - Automatically moves workspaces to source output and restores on exit
- **Scaling modes** - Fit, fill, stretch, or center content on target displays
- **Cursor capture** - Include or hide the cursor in the mirrored output
- **Clean shutdown** - Stop via `--stop` flag or Ctrl+C

## Requirements

- Sway or another wlroots-based compositor
- `zwlr_layer_shell_v1` protocol support
- At least one capture protocol:
  - `zwlr_export_dmabuf_manager_v1` (zero-copy)
  - `ext_image_copy_capture_manager_v1` (zero-copy when possible; Sway 1.11+ / wlroots 0.19+)
  - `zwlr_screencopy_manager_v1` (CPU copy)
- EGL/OpenGL ES 2.0 support

## GPU Support

| GPU | Status |
|-----|--------|
| AMD (Radeon, APUs) | Zero-copy, tested |
| Intel integrated and Arc (Xe/Xe2) | Zero-copy (tiled buffers via DRM format modifiers); Arc confirmation pending in #4 |
| NVIDIA (proprietary driver) | Zero-copy expected; Sway itself needs `--unsupported-gpu` |
| NVIDIA nouveau, ARM SoCs (Adreno, Mali, Raspberry Pi, Apple Silicon) | Zero-copy expected, incl. buffers that require `GL_OES_EGL_image_external` |
| Hybrid laptops / anything else | Falls back to CPU copy automatically if zero-copy fails |

If the mirror looks wrong on your GPU, please open an issue with the
`Capture method:` and `Capture format:` lines that sway-mirror prints at startup.
You can also try a different method with `--capture`.

## Build Dependencies

Install the required development libraries before building:

```bash
# Debian/Ubuntu
sudo apt install libwayland-dev libgbm-dev libegl1-mesa-dev libdrm-dev pkg-config

# Fedora/RHEL
sudo dnf install wayland-devel mesa-libgbm-devel mesa-libEGL-devel libdrm-devel

# Arch Linux
sudo pacman -S wayland mesa libdrm

# openSUSE
sudo zypper install wayland-devel libgbm-devel Mesa-libEGL-devel libdrm-devel
```

## Installation

```bash
# Build
cargo build --release

# Install to ~/.local/bin (user)
cp target/release/sway-mirror ~/.local/bin/

# Or install system-wide
sudo cp target/release/sway-mirror /usr/local/bin/
```

## Usage

```bash
# List available outputs
sway-mirror --list

# Mirror eDP-1 to all other outputs
sway-mirror eDP-1

# Mirror eDP-1 to specific output only
sway-mirror eDP-1 -t DP-7

# Mirror to multiple specific outputs
sway-mirror eDP-1 -t DP-7 -t DP-8

# Use fill scaling (crops to fill target)
sway-mirror eDP-1 -s fill

# Don't move workspaces
sway-mirror eDP-1 -w false

# Hide the cursor
sway-mirror eDP-1 --cursor false

# Force a capture method (default: auto)
sway-mirror eDP-1 --capture screencopy

# Stop a running instance
sway-mirror --stop
```

## Options

| Option | Description |
|--------|-------------|
| `SOURCE` | Source output to mirror (e.g., eDP-1, DP-7) |
| `-t, --to <OUTPUT>` | Target output(s). If not specified, mirrors to all other outputs |
| `-l, --list` | List available outputs and exit |
| `-s, --scale <MODE>` | Scaling mode: `fit` (default), `fill`, `stretch`, `center` |
| `-w, --workspaces [true\|false]` | Move all workspaces to source while mirroring (default: true) |
| `--cursor [true\|false]` | Include cursor in mirror (default: true) |
| `-c, --capture <METHOD>` | Capture method: `auto` (default), `export-dmabuf`, `image-copy`, `screencopy` |
| `--stop` | Stop a running sway-mirror instance |

## Scaling Modes

- **fit** - Preserve aspect ratio, fit within target (letterbox/pillarbox if needed)
- **fill** - Preserve aspect ratio, fill target completely (crops edges)
- **stretch** - Stretch to fill target, ignoring aspect ratio
- **center** - Display at 1:1 pixel ratio, centered (no scaling)

## Capture Methods

| Method | Protocol | Copy | Notes |
|--------|----------|------|-------|
| `export-dmabuf` | `wlr-export-dmabuf-unstable-v1` | Zero-copy | Fastest; tried first by `auto` |
| `image-copy` | `ext-image-copy-capture-v1` | Zero-copy, or CPU copy | Newer standard protocol; allocates GPU buffers via GBM, falls back to shared memory |
| `screencopy` | `wlr-screencopy-unstable-v1` | CPU copy | Works with any GPU/driver combination |

With `auto` (the default), sway-mirror uses the first available method in the
order above. If zero-copy capture or import fails at runtime (for example when
the compositor renders on a different GPU, or uses software rendering), it
switches to a CPU copy and keeps mirroring. A forced `--capture image-copy`
can still switch to shared memory on its own, but a forced method never
switches to a different protocol.

## How It Works

1. Captures frames from the source output (see [Capture Methods](#capture-methods))
2. Zero-copy: imports the DMA-BUF as an EGL image, passing its DRM format
   modifier and all planes so tiled/compressed layouts are read correctly.
   Layouts the driver can only sample as external textures use
   `GL_TEXTURE_EXTERNAL_OES`. CPU copy: uploads the shared-memory frame as a texture
3. Renders to layer-shell overlay surfaces on target outputs
4. Workspaces are moved to the source output so all content is visible in the mirror

## License

MIT
