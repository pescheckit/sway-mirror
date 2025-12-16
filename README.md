# sway-mirror

Fast zero-copy screen mirroring for Sway/wlroots compositors.

## Features

- **Zero-copy rendering** - Uses DMA-BUF for efficient GPU-to-GPU frame transfer
- **Multi-output support** - Mirror to one or all other outputs simultaneously
- **Workspace management** - Automatically moves workspaces to source output and restores on exit
- **Scaling modes** - Fit, fill, stretch, or center content on target displays
- **Cursor capture** - Optionally include the cursor in the mirrored output
- **Clean shutdown** - Stop via `--stop` flag or Ctrl+C

## Requirements

- Sway or wlroots-based compositor
- `zwlr_export_dmabuf_manager_v1` protocol support
- `zwlr_layer_shell_v1` protocol support
- EGL/OpenGL ES support

Works with both discrete GPUs and integrated graphics (Intel, AMD APU).

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
| `-w, --workspaces` | Move all workspaces to source while mirroring (default: true) |
| `--cursor` | Include cursor in mirror (default: true) |
| `--stop` | Stop a running sway-mirror instance |

## Scaling Modes

- **fit** - Preserve aspect ratio, fit within target (letterbox/pillarbox if needed)
- **fill** - Preserve aspect ratio, fill target completely (crops edges)
- **stretch** - Stretch to fill target, ignoring aspect ratio
- **center** - Display at 1:1 pixel ratio, centered (no scaling)

## How It Works

1. Captures frames from the source output using `zwlr_export_dmabuf_manager_v1`
2. Imports the DMA-BUF as an EGL image (zero-copy GPU texture)
3. Renders to layer-shell overlay surfaces on target outputs
4. Workspaces are moved to the source output so all content is visible in the mirror

## License

MIT
