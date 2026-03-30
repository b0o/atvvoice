# AGENTS.md - atvvoice

## Project Overview

**atvvoice** is a Rust userspace daemon that captures voice audio from BLE TV remotes using the Google Voice over BLE (ATVV) protocol and exposes it as a PipeWire virtual microphone source on Linux.

Target remotes: G20S Pro and any ATVV-compatible remote following the Google Reference Design. The daemon is generic - not tied to a specific remote model.

## Architecture

```
BLE Remote <--[BlueZ/D-Bus/GATT]--> atvvoice daemon --[PipeWire virtual source]--> Apps
```

Nine modules:

| Module            | File                     | Responsibility                                                         |
| ----------------- | ------------------------ | ---------------------------------------------------------------------- |
| BLE Discovery     | `src/ble.rs`             | Find ATVV devices, resolve GATT characteristics, AcquireNotify         |
| Protocol Types    | `src/protocol/types.rs`  | Strongly typed wire types: opcodes, codecs, stream IDs, reasons, etc.  |
| Protocol Trait    | `src/protocol/mod.rs`    | `Protocol` trait, `create_protocol()`, `get_caps_cmd()`, `parse_caps_resp()` |
| Protocol v0.4     | `src/protocol/v04.rs`    | v0.4 command encoding, CTL parsing, headered frame decoding            |
| Protocol v1.0     | `src/protocol/v10.rs`    | v1.0 command encoding, CTL parsing, headerless frame decoding, PTT/HTT |
| Session Loop      | `src/atvv.rs`            | Generic session state machine over `dyn Protocol`, `BleStreams` struct  |
| ADPCM Decoder     | `src/adpcm.rs`           | Stateful `AdpcmDecoder` struct + post-processing (declip, lowpass, gain) |
| PipeWire Source   | `src/pw.rs`              | Virtual audio source node (own thread, not async)                      |
| D-Bus Control     | `src/dbus.rs`            | Session bus interface for external mic control (optional feature)       |
| CLI / Main        | `src/main.rs`            | CLI parsing, `negotiate()`, tokio runtime, reconnect loop, signal handling |

## Tech Stack

- **Language:** Rust (2021 edition)
- **Async runtime:** tokio (multi-threaded)
- **BLE:** `bluer` 0.17 (async BlueZ D-Bus bindings, feature `bluetoothd`)
- **Audio:** `pipewire` 0.9 (`pipewire-rs` bindings)
- **CLI:** `clap` 4 (derive)
- **Protocol types:** `num_enum` 0.7 (TryFromPrimitive/IntoPrimitive), `bitflags` 2 (codec bitmask)
- **Logging:** `tracing` + `tracing-subscriber` (env-filter)
- **Async utilities:** `futures` 0.3
- **Build:** Nix flake (crane or naersk), also buildable with plain `cargo`
- **Platform:** Linux only (NixOS primary target)

## ATVV Protocol Reference

### GATT Service

Service UUID: `AB5E0001-5A21-4F05-BC7D-AF01F617B664`

| Characteristic | UUID suffix | Direction     | Purpose                           |
| -------------- | ----------- | ------------- | --------------------------------- |
| TX             | `AB5E0002`  | Host → Remote | Commands (write without response) |
| RX             | `AB5E0003`  | Remote → Host | Audio data (notify)               |
| CTL            | `AB5E0004`  | Remote → Host | Control signals (notify)          |

### Commands (Host → Remote, written to TX)

| Command   | Bytes                      | Notes                                                        |
| --------- | -------------------------- | ------------------------------------------------------------ |
| GET_CAPS   | `0x0A 0x01 0x00 0x00 0x03 0x03` | Always v1.0: version, reserved, codecs (8k+16k), models (PTT+HTT) |
| MIC_OPEN   | `0x0C 0x00 0x01`           | **Big-endian** codec. `0x01 0x00` is WRONG and gets rejected |
| MIC_CLOSE  | `0x0D`                     |                                                              |
| MIC_EXTEND | `0x0E 0x00`                | Reset audio transfer timeout. stream_id=0x00 for MIC_OPEN-initiated streams. No response expected. |

### Control Signals (Remote → Host, received on CTL)

| Signal        | First byte | Action                             |
| ------------- | ---------- | ---------------------------------- |
| AUDIO_STOP    | `0x00`     | Stop streaming, send MIC_CLOSE     |
| AUDIO_START   | `0x04`     | Begin streaming                    |
| START_SEARCH  | `0x08`     | Mic button pressed - send MIC_OPEN |
| GET_CAPS_RESP | `0x0B`     | Capabilities response (log only)   |

### Session Flow

**Negotiation phase** (in `main.rs::negotiate()`):
1. Subscribe to CTL, RX, and device event streams
2. Send GET_CAPS (always v1.0) → receive GET_CAPS_RESP
3. Parse CAPS_RESP to determine remote's protocol version and capabilities
4. Create version-specific Protocol from negotiated capabilities

**Session phase** (in `atvv::run_session()`):
5. User presses mic → START_SEARCH
6. Send MIC_OPEN → AUDIO_START
7. Audio frames stream on RX (~30.8 fps, 134 bytes each)
8. User releases mic → AUDIO_STOP (or second START_SEARCH)
9. Send MIC_CLOSE

**Important:** Some remotes send a second START_SEARCH instead of AUDIO_STOP when the mic button is released. The daemon supports two modes via `--mode`:

- **toggle** (default): START_SEARCH while streaming = stop. Next press starts fresh.
- **hold**: START_SEARCH while streaming = stop + immediate re-open (for remotes with hold-to-talk).

### Audio Frame Format (134 bytes)

```
Bytes 0-1:   Sequence ID (big-endian, monotonically increasing)
Byte 2:      0x00 (padding/reserved)
Bytes 3-4:   DVI predictor value (big-endian, signed 16-bit)
Byte 5:      DVI step table index (0-88)
Bytes 6-133: 128 bytes IMA/DVI ADPCM nibbles (high nibble first)
```

- **6-byte header:** 3 bytes app (seq + padding) + 3 bytes DVI (predictor + step_index)
- **Per-frame decoder reset:** The decoder MUST reset predictor and step_index from the DVI header at the start of each frame. Without this, the decoder diverges (step_index explodes to 73+, predictor drifts to -13000+).
- **257 samples per frame:** 1 predictor (sample 0) + 256 decoded from 128 ADPCM bytes
- **8kHz sample rate**, 16-bit mono, ~32ms per frame
- **High nibble decoded first** within each byte

### IMA/DVI ADPCM Codec

Standard IMA/DVI ADPCM (Intel 1992 spec):

- 89-entry step table, 8-entry index table (`[-1, -1, -1, -1, 2, 4, 6, 8]`)
- Nibble format: bit 3 = sign, bits 2-0 = magnitude
- Diff calculation: `diff = step >> 3; if bit0: += step>>2; if bit1: += step>>1; if bit2: += step`
- Predictor clamped to -32768..32767, step_index clamped to 0..88

### Audio Post-Processing Pipeline

Applied after ADPCM decode, before PipeWire output:

1. **Click removal (declip):** Interpolate single-sample spikes where `|cur-prev| > 1000 AND |cur-next| > 1000 AND min(dp,dn) > |next-prev| * 2`
2. **Low-pass filter:** 3-tap triangle `[0.25, 0.5, 0.25]` - removes high-freq quantization noise
3. **Gain:** Fixed dB gain (default 20dB ≈ 10x). Remote mic produces very low-level output.

## Threading & Channel Architecture

```
tokio (single-threaded) ──────────────────────────────────────────
  │                                                               │
  ├─ BLE/ATVV task ──[tokio::mpsc<Vec<u8>>]──> Decoder task ─────┤
  │                                               │               │
  └───────────────────────────────────────────────┼───────────────┘
                                                  │
                                      [std::sync::mpsc<Vec<i16>>]
                                                  │
  PipeWire thread (std::thread::spawn) ───────────┘
```

- `tokio::sync::mpsc` bridges ATVV → decoder (both in tokio)
- `std::sync::mpsc` bridges decoder → PipeWire (PipeWire has its own non-async main loop)
- PipeWire MUST run on a separate OS thread - `pipewire-rs` is not tokio-compatible

## Key Technical Decisions & Gotchas

1. **MIC_OPEN byte order is big-endian.** Sending LE (`0x01 0x00`) results in error `0x0C 0x0F 0x01`. Must send `0x00 0x01`.

2. **Per-frame DVI state reset is essential.** The encoder embeds predictor + step_index in each frame's 3-byte DVI header. The decoder must use these, not carry state between frames.

3. **`bluer::Error` may not have public constructors.** Use `anyhow` or a custom error type for fallible operations in `ble.rs`.

4. **PipeWire `Direction::Output` = audio source.** Confusing but correct - "output" means the stream outputs data TO PipeWire (appearing as a microphone).

5. **`tokio::signal::ctrl_c()` is consumed after first await.** In the reconnection loop, create it outside the loop or re-create each iteration.

6. **`chars` must be `let mut`** to allow re-assignment after BLE reconnection.

7. **BLE MTU is 140** on the test system. Full 134-byte frames fit in a single GATT notification. No sub-packet reassembly needed.

8. **GET_CAPS_RESP `bytes_per_char=20`** describes behavior at default MTU=23, not actual behavior at negotiated MTU.

9. **PipeWire mono channel position must be set explicitly.** Without `set_position([SPA_AUDIO_CHANNEL_MONO, ...])`, PipeWire defaults to FL (front left) and audio only plays on the left channel.

10. **G20S Pro sends START_SEARCH on both press and release** - it does not maintain a "held" state. Toggle mode is the correct default for this remote.

11. **Device goes to sleep after inactivity.** Stops sending frames without any CTL signal. The `--frame-timeout` detects this and auto-closes the mic so the next button press works cleanly (no double-press needed).

12. **Sample rate (8kHz) and channel count (mono) are implied by codec 0x0001** (ADPCM). They are not separately negotiated in GET_CAPS_RESP.

13. **Protocol version is auto-negotiated.** GET_CAPS always sends v1.0 (our max version). The remote's CAPS_RESP version field determines which protocol implementation is used. v0.4 remotes tolerate the extra interaction_models byte in v1.0 GET_CAPS. The `--protocol-version` CLI flag was removed.

## Development Environment

- **User:** `boo` on NixOS
- **Workspace:** `/home/boo/proj/atvvoice/worktree/main/` (bare git repo + worktree)
- **BLE adapter:** `hci0`
- **Test remote:** G20S Pro at `69:98:98:22:FF:7B` (already bonded)
- **Audio stack:** PipeWire (not PulseAudio directly)
- **Python:** Not directly available - use `nix shell nixpkgs#python3`
- **Dotfiles:** `/home/boo/dotfiles/` (Nix flake with Home Manager)

## Build & Test

```bash
cargo check                                                               # Type-check
cargo test                                                                # Run all tests
cargo test adpcm                                                          # Run ADPCM decoder tests only
cargo build                                                               # Debug build
cargo run -- -d AA:BB:CC:DD:EE:FF -v                                      # Run with test remote
cargo run -- -d AA:BB:CC:DD:EE:FF -v -m hold                              # Hold-to-talk mode
cargo run -- -d AA:BB:CC:DD:EE:FF -v --frame-timeout 5 --idle-timeout 300 # With timeouts
nix build                                                                 # Nix build
```

## Build & Test (with nix)

```bash
nix develop --command cargo check                                                               # Type-check
nix develop --command cargo test                                                                # Run all tests
nix develop --command cargo clippy --tests -- -W clippy::all                                    # Lint
nix develop --command cargo run -- -d AA:BB:CC:DD:EE:FF -v                                      # Run with test remote
nix develop --command cargo run -- -d AA:BB:CC:DD:EE:FF -v -m hold                              # Hold-to-talk mode
nix develop --command cargo run -- -d AA:BB:CC:DD:EE:FF -v --frame-timeout 5 --idle-timeout 300 # With timeouts
nix build                                                                                       # Nix build
```

## Reference Documents

| Document            | Path                                               | Purpose                                    |
| ------------------- | -------------------------------------------------- | ------------------------------------------ |
| Design Spec         | `docs/specs/2026-03-23-atvvoice-design.md`         | Full architecture and protocol spec        |
| Implementation Plan | `docs/plans/2026-03-23-atvvoice-implementation.md` | Task-by-task build plan with code snippets |
| Research Report     | `docs/research/report.md`                          | Protocol reverse-engineering findings      |
| Python PoC          | `docs/research/scripts/atvv-capture.py`            | Working reference implementation           |
| Decode Test         | `docs/research/scripts/decode-test.py`             | Multi-strategy ADPCM decoder comparison    |

## External References

- [BlueZ issue #1086](https://github.com/bluez/bluez/issues/1086) - Linux support request, protocol research
- [Infineon CYW20829 Voice Remote](https://github.com/Infineon/mtb-example-btstack-freertos-cyw20829-voice-remote) - Reference firmware (source of truth for frame format)
- [CSDN ATVV spec translation](https://blog.csdn.net/Weichen_Huang/article/details/109251338) - Command table, characteristic UUIDs
- [Google Voice over BLE spec v1.0](https://web.archive.org/web/20260324183034/https://wangefan.github.io/linux_kernel_driver/resources/Google_Voice_over_BLE_spec_v1.0.pdf) - Official spec

## Implementation Plan

The implementation follows 8 sequential tasks with dependencies. Read `docs/plans/2026-03-23-atvvoice-implementation.md` for full task details with code snippets.

| Task | Description                                       | Dependencies     |
| ---- | ------------------------------------------------- | ---------------- |
| 1    | Project scaffold (Cargo.toml, flake.nix, main.rs) | None             |
| 2    | ADPCM decoder module with tests                   | Task 1           |
| 3    | BLE discovery module                              | Task 1           |
| 4    | ATVV protocol state machine                       | Tasks 1, 3       |
| 5    | PipeWire audio source                             | Task 1           |
| 6    | Main entry point & integration                    | Tasks 2, 3, 4, 5 |
| 7    | Integration test with real hardware               | Task 6           |
| 8    | Nix packaging & Home Manager module               | Task 6           |

## CI & Release

**CI** (`.github/workflows/ci.yml`): Runs on push to `main` and PRs.

- `nix flake check` - evaluates all flake outputs, builds, runs tests
- `nix build` - builds release binary
- `nix develop --command cargo test` - runs test suite

**Release** (`.github/workflows/release.yml`): Triggered by `v*` tag push or manual dispatch.

- Builds `x86_64-linux` and `aarch64-linux` binaries via Nix (aarch64 uses QEMU)
- Creates GitHub release with binaries and auto-generated release notes

**COPR** (`.github/workflows/copr.yml`): Triggered on release publish and manual dispatch.

- Runs in `fedora:latest` container
- `packaging/rpm/build-srpm.sh` creates SRPM with vendored Cargo deps
- `copr-cli build` submits to Fedora COPR for Fedora 42/43/44 (x86_64, aarch64)
- Secrets: `COPR_API_LOGIN`, `COPR_API_TOKEN` from copr.fedorainfracloud.org

### Release process

1. Bump version in `Cargo.toml`
2. Run `cargo check` to update `Cargo.lock` (Nix builds with `--locked`)
3. Commit both files: `git add Cargo.toml Cargo.lock && git commit -m "chore: bump version to vX.Y.Z"`
4. Tag: `git tag -s vX.Y.Z -m "vX.Y.Z\n\n- change 1\n- change 2"`
5. Push: `git push origin main --tags`
6. CI builds and creates the GitHub release automatically

**Important:** Always commit `Cargo.lock` after version bumps. Nix uses `--locked` and will fail if `Cargo.lock` doesn't match `Cargo.toml`.

## Session Preferences

- **Commits:** Do NOT commit unless explicitly asked
- **TDD:** Required for all implementation tasks. Write tests first, watch them fail, then implement.
- **Workflow:** Use `superpowers:subagent-driven-development` to execute plan tasks

## Non-Goals

- Pairing/bonding (use `bluetoothctl` manually)
- Multiple simultaneous remotes
- ALSA fallback (PipeWire only)
- Opus codec (ADPCM only, codec 0x0001)
- Android TV emulation
