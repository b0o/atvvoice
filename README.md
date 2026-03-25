# ATVVoice

Linux daemon that captures voice audio from BLE TV remotes using the [Android TV Voice over BLE (ATVV)](https://web.archive.org/web/20260324183034/https://wangefan.github.io/linux_kernel_driver/resources/Google_Voice_over_BLE_spec_v1.0.pdf) protocol and exposes it as a PipeWire virtual microphone.

### Supported devices

| Device | Status |
|--------|--------|
| G20S Pro / G20S Pro Plus / G20BTS Plus | Verified working |
| Other ATVV-compatible remotes | Unknown |

If you have a remote you'd like to test, open an issue with the device name, Bluetooth address, and output of `atvvoice -d <ADDR> -vv`. See [docs/research/report.md](docs/research/report.md) for protocol details.

## Requirements

- Linux with BlueZ and PipeWire
- A bonded ATVV-compatible BLE remote (pair with `bluetoothctl`)

## Installation

### Nix flake

```nix
# flake.nix
inputs.atvvoice = {
  url = "github:b0o/atvvoice";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

**Home Manager module:**

```nix
imports = [ inputs.atvvoice.homeManagerModules.atvvoice ];

# minimal - auto-detects first ATVV device
services.atvvoice.enable = true;
```

**As overlay:**

```nix
nixpkgs.overlays = [ inputs.atvvoice.overlays.default ];
# then: pkgs.atvvoice
```

### Cargo

```
cargo install --path .
```

Requires `pipewire` and `dbus` development libraries and `pkg-config`.

## Usage

```
atvvoice [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-d, --device` | auto | Bluetooth address of remote. Auto-detects first ATVV device if omitted. |
| `-a, --adapter` | auto | BlueZ adapter name |
| `-g, --gain` | 20 | Audio gain in dB |
| `-m, --mode` | toggle | `toggle` (press on/off) or `hold` (hold to stream)\* |
| `--frame-timeout` | 5 | Seconds without frames before auto-closing mic (device asleep). 0 = disabled. |
| `-t, --idle-timeout` | 0 | Seconds since last button press before auto-closing mic. 0 = disabled. |
| `-n, --name` | | Instance name suffix. Sets PW node and D-Bus name (e.g. `--name living-room`). |
| `--description` | BLE device name | PipeWire node description (shown in audio settings). Defaults to the remote's BLE name (e.g. "G20S PRO"). |
| `--no-dbus` | | Disable D-Bus control interface |
| `-v` | off | Verbosity (`-v` debug, `-vv` trace) |

\*Not all remotes support hold-to-stream. The G20S Pro sends a button press event on both press and release, so it only works in toggle mode.

The remote appears by its BLE device name (e.g. "G20S PRO") in PipeWire/PulseAudio audio input settings. The microphone source appears when the device connects and disappears when it disconnects. ATVVoice automatically reconnects when the device comes back.

### Multiple remotes

Each ATVVoice instance handles one remote. To use multiple remotes, run separate instances with `--name` to avoid PipeWire and D-Bus collisions:

```
atvvoice -d AA:BB:CC:DD:EE:FF --name living-room &
atvvoice -d 11:22:33:44:55:66 --name office &
```

This creates PW nodes `atvvoice-living-room` / `atvvoice-office` and D-Bus names `org.atvvoice.living-room` / `org.atvvoice.office`.

## Home Manager options

All options default to `null`, deferring to the app's built-in defaults. Only explicitly set values are passed as CLI flags.

```nix
services.atvvoice = {
  enable = true;

  # Bluetooth address. null (default) = auto-detect first ATVV device.
  device = "AA:BB:CC:DD:EE:FF";

  # BlueZ adapter name. null (default) = auto-detect.
  adapter = null;

  # Audio gain in dB. null (default) = 20.
  gain = 20;

  # "toggle" = press on/off. "hold" = hold to stream.
  # Not all remotes support hold mode. null (default) = toggle.
  mode = "toggle";

  # Seconds without audio frames before auto-closing mic. 0 = disabled.
  # null (default) = 5.
  frameTimeout = 5;

  # Seconds since last button press before auto-closing mic. 0 = disabled.
  # null (default) = 0.
  idleTimeout = 300;

  # Log verbosity: 0 = info, 1 = debug, 2+ = trace. null (default) = 0.
  verbose = 1;

  # Instance name suffix. Sets PW node name (atvvoice-<name>) and D-Bus name
  # (org.atvvoice.<name>). null (default) = derived from BLE device name.
  name = null;

  # PipeWire node description (shown in audio settings).
  # null (default) = BLE device name (e.g. "G20S PRO").
  description = null;

  # Disable D-Bus control interface. Default: false.
  noDbus = false;
};
```

## D-Bus control interface

When built with the `dbus` feature (enabled by default), ATVVoice exposes `org.atvvoice.<name>` on the session bus, where `<name>` is the instance name (auto-derived from BLE device name or set via `--name`). Disable at runtime with `--no-dbus`.

```
# Toggle mic on/off (replace g20s-pro with your instance name)
busctl --user call org.atvvoice.g20s-pro /org/atvvoice/Daemon org.atvvoice.Daemon MicToggle

# Query state
busctl --user get-property org.atvvoice.g20s-pro /org/atvvoice/Daemon org.atvvoice.Daemon State

# Monitor state changes
busctl --user monitor org.atvvoice.g20s-pro
```

| Methods | Description |
|---------|-------------|
| `MicOpen` | Start streaming |
| `MicClose` | Stop streaming |
| `MicToggle` | Toggle based on current state |

| Properties | Type | Description |
|------------|------|-------------|
| `State` | `s` | `"init"`, `"ready"`, `"opening"`, `"streaming"` |
| `DeviceAddress` | `s` | BT address of connected remote |
| `NodeName` | `s` | PipeWire node name |

| Signals | Args | Description |
|---------|------|-------------|
| `MicStateChanged` | `s` | Emitted on state transitions (new state value) |

To build without D-Bus support entirely: `cargo build --no-default-features`

## How it works

```
BLE Remote --[GATT/ATVV]--> atvvoice --[PipeWire]--> Apps
```

1. Discovers and connects to the remote via BlueZ D-Bus
2. Subscribes to ATVV GATT notifications (audio + control)
3. On mic button press: sends MIC_OPEN, receives IMA/DVI ADPCM audio frames
4. Decodes ADPCM, applies click removal + lowpass filter + gain
5. Outputs 8kHz 16-bit mono PCM to a PipeWire virtual source
6. On device disconnect: removes PipeWire source, waits for reconnect

ATVVoice implements ATVV protocol v0.4. The [v1.0 spec](https://web.archive.org/web/20260324183034/https://wangefan.github.io/linux_kernel_driver/resources/Google_Voice_over_BLE_spec_v1.0.pdf) adds PTT/HTT interaction models, headerless audio frames, and stream IDs - these are not yet supported.

See [docs/research/report.md](docs/research/report.md) for the full protocol reverse-engineering writeup and [docs/specs/2026-03-23-atvvoice-design.md](docs/specs/2026-03-23-atvvoice-design.md) for the design spec.

## License

MIT
