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

### Debian/Ubuntu (.deb)

Download the `.deb` package for your architecture from the [latest release](https://github.com/b0o/atvvoice/releases/latest):

```bash
sudo dpkg -i atvvoice_*_amd64.deb   # x86_64
sudo dpkg -i atvvoice_*_arm64.deb   # aarch64
```

This installs a systemd user service. See [Systemd service](#systemd-service) for setup.

### [Fedora (COPR)](https://copr.fedorainfracloud.org/coprs/maddison-io/ATVVoice/)

```bash
sudo dnf copr enable maddison-io/ATVVoice
sudo dnf install atvvoice
```

Available for Fedora 42+ on x86_64 and aarch64. This installs a systemd user service. See [Systemd service](#systemd-service) for setup.

### Fedora/RHEL (.rpm)

Download the `.rpm` package for your architecture from the [latest release](https://github.com/b0o/atvvoice/releases/latest):

```bash
sudo rpm -i atvvoice-*.x86_64.rpm    # x86_64
sudo rpm -i atvvoice-*.aarch64.rpm   # aarch64
```

This installs a systemd user service. See [Systemd service](#systemd-service) for setup.

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
| `--frame-timeout` | 5 | Seconds without frames before auto-closing mic (device asleep). 0 = disabled. |
| `-t, --idle-timeout` | 0 | Seconds since last mic button press before auto-closing mic. Only resets on the voice/assistant button, not other remote buttons. 0 = disabled. |
| `--keep-alive` | 10 | Seconds between keepalive messages to prevent the remote's audio transfer timeout. 0 = disabled. See [Audio keepalive](#audio-keepalive). |
| `-n, --name` | | Instance name suffix. Sets PW node and D-Bus name (e.g. `--name living-room`). |
| `--description` | BLE device name | PipeWire node description (shown in audio settings). Defaults to the remote's BLE name (e.g. "G20S PRO"). |
| `--no-dbus` | | Disable D-Bus control interface |
| `-v` | off | Verbosity (`-v` debug, `-vv` trace) |

The remote appears by its BLE device name (e.g. "G20S PRO") in PipeWire/PulseAudio audio input settings. The microphone source appears when the device connects and disappears when it disconnects. ATVVoice automatically reconnects when the device comes back.

### Audio keepalive

ATVV remotes have a hardware "Audio Transfer Timeout" (typically 15-60 seconds) to prevent battery drain when the host fails to close the mic. Without intervention, the remote stops streaming after this timeout expires.

ATVVoice sends periodic keepalive messages to reset this timer, allowing audio sessions to run indefinitely. The keepalive method depends on the remote's protocol version:

| Protocol version | Keepalive method | Behavior |
|-----------------|-----------------|----------|
| v1.0+ | `MIC_EXTEND` | Silent - no response from remote, no stream disruption |
| v0.4 | `MIC_OPEN` (fallback) | Remote sends `AUDIO_START` and resets its sequence counter, but audio data continues uninterrupted. |

The protocol version is auto-detected from the remote's `CAPS_RESP` message.

Set `--keep-alive 0` to disable keepalive entirely (audio will stop at the remote's hardware timeout, typically ~30 seconds).

### Multiple remotes

Each ATVVoice instance handles one remote. To use multiple remotes, run separate instances with `--name` to avoid PipeWire and D-Bus collisions:

```
atvvoice -d AA:BB:CC:DD:EE:FF --name living-room &
atvvoice -d 11:22:33:44:55:66 --name office &
```

This creates PW nodes `atvvoice-living-room` / `atvvoice-office` and D-Bus names `org.atvvoice.living-room` / `org.atvvoice.office`.

## Systemd service

The Linux packages include a systemd user service. If you installed via Cargo or a bare binary, you can grab the [service file](dist/atvvoice.service) directly and install it to `~/.config/systemd/user/`. Enable it after installation:

```bash
systemctl --user enable --now atvvoice
```

By default, the service runs `atvvoice` with no arguments, which auto-detects the first ATVV device. To customize options (e.g., set a specific device address or gain), create a drop-in override:

```bash
systemctl --user edit atvvoice
```

Then add:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/atvvoice --device AA:BB:CC:DD:EE:FF --gain 25
```

Note: The blank `ExecStart=` line is required. It clears the default command before setting a new one. Without it, systemd runs both the original and your custom command.

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

  # Seconds without audio frames before auto-closing mic. 0 = disabled.
  # null (default) = 5.
  frameTimeout = 5;

  # Seconds since last mic button press before auto-closing mic.
  # Only resets on the voice/assistant button, not other remote buttons.
  # 0 = disabled. null (default) = 0.
  idleTimeout = 300;

  # Seconds between keepalive messages. 0 = disabled.
  # null (default) = 10.
  keepAlive = 10;

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
| `State` | `s` | `"disconnected"`, `"connected"`, `"opening"`, `"streaming"` |
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
3. Exchanges capabilities (GET_CAPS / CAPS_RESP) to detect protocol version
4. On mic button press: sends MIC_OPEN, receives IMA/DVI ADPCM audio frames
5. Decodes ADPCM, applies click removal + lowpass filter + gain
6. Outputs 8kHz or 16kHz 16-bit mono PCM to a PipeWire virtual source (matches negotiated codec)
7. Sends periodic keepalive messages (MIC_EXTEND or MIC_OPEN) to prevent the remote's audio transfer timeout
8. On device disconnect: removes PipeWire source, waits for reconnect

ATVVoice supports ATVV protocol v0.4 and v1.0. The protocol version is auto-detected from the remote's CAPS_RESP. v0.4 devices (like the G20S Pro) use MIC_OPEN as a keepalive fallback; v1.0+ devices use the dedicated MIC_EXTEND command.

See [docs/research/report.md](docs/research/report.md) for the full protocol reverse-engineering writeup and [docs/specs/2026-03-23-atvvoice-design.md](docs/specs/2026-03-23-atvvoice-design.md) for the design spec.

## License

MIT
