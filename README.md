# ATVVoice

Linux daemon that captures voice audio from BLE TV remotes using the [Android TV Voice over BLE (ATVV)](https://ralexeev.github.io/smart_remote_3_nrf52/html/group__ble__atvv.html) protocol and exposes it as a PipeWire virtual microphone.

### Supported devices

| Device | Status |
|--------|--------|
| G20S Pro / G20S Pro Plus / G20BTS Plus | Verified working |
| UR02 | Should work, untested |
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

# minimal — auto-detects first ATVV device
services.atvvoice.enable = true;
```

**As overlay:**

```nix
nixpkgs.overlays = [ inputs.atvvoice.overlays.default ];
# then: pkgs.atvvoice
```

### Pre-built binary

Download from [GitHub Releases](https://github.com/b0o/atvvoice/releases):

```
curl -Lo atvvoice https://github.com/b0o/atvvoice/releases/latest/download/atvvoice-x86_64-linux
chmod +x atvvoice
sudo mv atvvoice /usr/local/bin/
```

Replace `x86_64-linux` with `aarch64-linux` for ARM64.

Requires `libpipewire` and `libdbus` at runtime.

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
| `--node-name` | atvvoice | PipeWire node name |
| `--node-description` | ATVVoice Microphone | PipeWire node description (shown in audio settings) |
| `--no-dbus` | | Disable D-Bus control interface |
| `-v` | off | Verbosity (`-v` debug, `-vv` trace) |

\*Not all remotes support hold-to-stream. The G20S Pro sends a button press event on both press and release, so it only works in toggle mode.

The remote appears as "ATVVoice Microphone" in PipeWire/PulseAudio audio input settings. The microphone source appears when the device connects and disappears when it disconnects. ATVVoice automatically reconnects when the device comes back.

### Multiple remotes

Each ATVVoice instance handles one remote. To use multiple remotes, run separate instances with different `-d` addresses and `--node-name`/`--node-description` to avoid PipeWire naming collisions:

```
atvvoice -d AA:BB:CC:DD:EE:FF --node-name atvvoice-living-room --node-description "Living Room Remote" &
atvvoice -d 11:22:33:44:55:66 --node-name atvvoice-bedroom --node-description "Bedroom Remote" &
```

Note: the D-Bus bus name (`org.atvvoice`) can only be claimed by one instance. Use `--no-dbus` on additional instances. Per-device bus names may be added in the future. If you need better multi-remote support, please open an issue describing your setup.

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

  # PipeWire node name. null (default) = "atvvoice".
  nodeName = null;

  # PipeWire node description (shown in audio settings).
  # null (default) = "ATVVoice Microphone".
  nodeDescription = null;

  # Disable D-Bus control interface. Default: false.
  noDbus = false;
};
```

## D-Bus control interface

When built with the `dbus` feature (enabled by default), ATVVoice exposes `org.atvvoice.Daemon` on the session bus. Disable at runtime with `--no-dbus`.

```
# Toggle mic on/off
busctl --user call org.atvvoice /org/atvvoice/Daemon org.atvvoice.Daemon MicToggle

# Query state
busctl --user get-property org.atvvoice /org/atvvoice/Daemon org.atvvoice.Daemon State

# Monitor state changes
busctl --user monitor org.atvvoice
```

| Methods | Description |
|---------|-------------|
| `MicOpen` | Start streaming |
| `MicClose` | Stop streaming |
| `MicToggle` | Toggle based on current state |

| Properties | Type | Description |
|------------|------|-------------|
| `State` | `s` | `"ready"`, `"opening"`, `"streaming"` |
| `DeviceAddress` | `s` | BT address of connected remote |
| `NodeName` | `s` | PipeWire node name |

| Signals | Args | Description |
|---------|------|-------------|
| `MicStateChanged` | `s` | Emitted on state transitions |

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

See [docs/research/report.md](docs/research/report.md) for the full protocol reverse-engineering writeup and [docs/specs/2026-03-23-atvvoice-design.md](docs/specs/2026-03-23-atvvoice-design.md) for the design spec.

## License

MIT
