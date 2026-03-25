# Reverse Engineering the G20S Pro BLE Voice Protocol

**Date:** March 2026
**Device:** G20S PRO BLE Remote (VID `1d5a`, PID `c081`)
**BT Address:** `AA:BB:CC:DD:EE:FF` (example)
**Host:** NixOS, BlueZ, PipeWire

## Motivation

The G20S Pro is a cheap Android TV remote with a built-in microphone. It connects
via both 2.4GHz proprietary RF (USB dongle) and Bluetooth Low Energy. The goal
was to get the microphone working over BLE on Linux, bypassing the USB dongle for
security reasons - BLE is encrypted via AES-128-CCM when bonded, while the
2.4GHz link is likely unencrypted.

The remote pairs and works for keyboard/mouse/gyro input over BLE, but the
microphone does not appear as an audio device. No HSP, HFP, or A2DP profiles
are advertised. The mic uses a vendor-specific BLE protocol that Linux has no
support for.

## Phase 1: Reconnaissance

### What the device advertises

After bonding via `bluetoothctl`, the device exposes standard BLE services plus
two vendor-specific ones:

| Service            | UUID                                       |
| ------------------ | ------------------------------------------ |
| Generic Access     | `00001800`                                 |
| Device Information | `0000180a`                                 |
| Battery            | `0000180f`                                 |
| HID (HoGP)         | `00001812`                                 |
| Vendor (FFF0)      | `0000fff0`                                 |
| **Vendor (ATVV)**  | **`ab5e0001-5a21-4f05-bc7d-af01f617b664`** |

The `ab5e0001` service turned out to be the key - it's Google's ATV Voice
Service, though we didn't know that yet.

### HID Report Descriptor

We read the HID Report Map from the HoGP service:

```bash
cat /sys/class/hidraw/hidrawN/device/report_descriptor | hidrd-convert -o spec
```

This revealed five report IDs:

| Report ID     | Usage Page                 | Purpose                           |
| ------------- | -------------------------- | --------------------------------- |
| 1             | Generic Desktop (Keyboard) | Standard 8-byte keyboard report   |
| 2             | Generic Desktop (Mouse)    | 3 buttons + X/Y/scroll            |
| 3             | Consumer Control           | 2× 16-bit consumer keys           |
| 5             | System Control             | Power Down / Sleep                |
| **30 (0x1E)** | **Vendor 0xFF01**          | **20 bytes input, 1 byte output** |

Report ID 30 on a vendor usage page matched the pattern used by the Android
`hid-atv-remote.c` driver[^1] for ADPCM audio embedded in HID reports. This was
a red herring - our remote doesn't use this report for audio (see below).

### Checking for audio in HID reports

We monitored the hidraw device while pressing the mic button:

```bash
sudo timeout 15 xxd /dev/hidrawN
```

**Result:** The mic button only produced Consumer Control report ID 3 with usage
`0x00CF` ("Voice Command"). No data appeared on Report ID 30. Every other button
press produced the same `03 cf 00 00` / `03 00 00 00` toggle pattern. No audio
data was flowing through HID.

This ruled out VoHoGP (Voice over HID over GATT Profile)[^2] - the method used
by older Google remotes (ADT-1, Nexus Player) where audio is embedded in HID
reports. Our remote uses VoGP (Voice over GATT Profile), where audio flows
through a dedicated GATT service.

## Phase 2: Identifying the Protocol

### The ab5e0001 service

Searching for the UUID `ab5e0001-5a21-4f05-bc7d-af01f617b664` led to BlueZ
issue #1086[^3], where another user had the exact same problem with a UR02
remote. The issue identified the service as Google's **ATV Voice Service (ATVV)**,
part of the "Google Voice over BLE" specification.

The ATVV service has three characteristics:

| Characteristic | UUID       | Properties | Purpose                         |
| -------------- | ---------- | ---------- | ------------------------------- |
| ATVV_CHAR_TX   | `ab5e0002` | Write      | Host → Remote commands          |
| ATVV_CHAR_RX   | `ab5e0003` | Notify     | Audio data (Remote → Host)      |
| ATVV_CHAR_CTL  | `ab5e0004` | Notify     | Control signals (Remote → Host) |

The CSDN blog translation of the spec[^4] provided the command table:

| Command       | Byte(s)                         | Direction     |
| ------------- | ------------------------------- | ------------- |
| GET_CAPS      | `0x0A` + version(2) + codecs(2) | Host → Remote |
| MIC_OPEN      | `0x0C` + codec(2)               | Host → Remote |
| MIC_CLOSE     | `0x0D`                          | Host → Remote |
| GET_CAPS_RESP | `0x0B` + caps(8)                | Remote → Host |
| START_SEARCH  | `0x08`                          | Remote → Host |
| AUDIO_START   | `0x04`                          | Remote → Host |
| AUDIO_STOP    | `0x00`                          | Remote → Host |

### First attempt: wrong D-Bus interface

We wrote a Python script to enable GATT notifications and send commands via
BlueZ D-Bus. The first version failed silently - no notifications received
despite `btmon` showing they were arriving at the BLE level.

**Root cause:** The D-Bus properties interface was specified as
`"dbus.freedesktop.Properties"` instead of the correct
`"org.freedesktop.DBus.Properties"`. After fixing this, notifications started
flowing.

### GET_CAPS handshake works

With the D-Bus fix, we successfully exchanged capabilities:

```
[TX] GET_CAPS: 0a 00 01 00 01
[CTL] GET_CAPS_RESP: 0b 00 04 00 01 00 86 00 14
```

Decoded GET_CAPS_RESP:

- Version: 0.4
- Codecs supported: `0x0001` (ADPCM 8kHz)
- Bytes per frame: 134 (`0x0086`)
- Bytes per characteristic: 20 (`0x0014`)

### MIC_OPEN rejected: byte order bug

When the mic button was pressed, the remote sent `START_SEARCH` (`0x08`) on
CHAR_CTL. We responded with `MIC_OPEN`: `0x0C 0x01 0x00`.

The remote responded with `0x0C 0x0F 0x01` - an error. No audio followed.

**Root cause:** The ATVV protocol uses big-endian multi-byte fields. The codec
value `0x0001` (ADPCM 8kHz) must be sent as `0x00 0x01`, not `0x01 0x00`. We
were sending codec value `0x0100` (256), which the remote rejected.

After fixing to `0x0C 0x00 0x01`, the remote responded with `AUDIO_START`
(`0x04`) and began streaming 134-byte audio frames on CHAR_RX.

## Phase 3: Decoding the Audio

### Initial decode attempts

We knew the codec was IMA/DVI ADPCM at 8kHz from the GET_CAPS_RESP. The
question was the frame format - specifically the header structure and nibble
packing order.

We tried many combinations systematically:

| Strategy | Header               | Nibble Order | State      | Quality                           |
| -------- | -------------------- | ------------ | ---------- | --------------------------------- |
| A        | 2-byte seq           | High-first   | Continuous | ~40% - clicks at frame boundaries |
| B        | 2-byte seq           | Low-first    | Continuous | ~25% - barely legible             |
| C        | 4-byte (seq+pred BE) | High-first   | Pred reset | ~35%                              |
| H        | 4-byte (seq+pred LE) | High-first   | Pred reset | ~40%                              |
| I        | 4-byte (skip)        | High-first   | Continuous | ~45% - best so far                |
| K        | 4-byte               | High-first   | Full reset | Very quiet                        |

All strategies produced recognizable but significantly distorted speech. The
nibble distribution was verified to be textbook-perfect for IMA ADPCM (symmetric,
exponentially decreasing magnitude), confirming the data was valid ADPCM. Our
decoder was verified correct via encode/decode round-trip test (28.2 dB SNR on
a 440Hz sine wave).

### Structural analysis of the frame

We analyzed byte-level patterns across all captured frames:

```
Byte 2: ALWAYS 0x00 (1 unique value across 95 frames)
Byte 3: only 5 unique values: {0, 2, 251, 254, 255}
All other byte positions: 59-69 unique values (random ADPCM data)
```

Byte 2 being constant and byte 3 having only 5 values proved these were metadata,
not ADPCM data. But we still couldn't determine the exact header structure.

### Step index explosion

Detailed decoder state analysis revealed the problem:

```
Frame  0: pred=     0  idx= 0  step=    7
Frame  2: pred=    -1  idx=14  step=   28
Frame  3: pred=-13339  idx=49  step=  796   ← exploding!
Frame 14: pred=   -25  idx=73  step= 7845  ← completely diverged
```

The step index was growing uncontrollably (reaching 73 = step size 7845), and the
predictor drifted to -13339. This meant 5,689 out of ~24,000 samples had jumps
exceeding 5,000 - the decoder was diverging from the encoder's state.

### The breakthrough: Infineon reference firmware

Searching for the `ab5e0001` UUID and ADPCM on GitHub led to the Infineon
CYW20829 Voice Remote reference implementation[^5]. This is the reference
firmware for the exact chip family used in these remotes.

Reading the source code (`cy_adpcm.c`, `adpcm.c`, `app_bt_hid_atv.c`) revealed
the **actual** frame format:

```c
// app_bt_hid_atv.c:493-520
enc_buffer[0] = (g_seq_id >> 8) & 0xFF;  // seq high byte
enc_buffer[1] = g_seq_id & 0xFF;          // seq low byte
enc_buffer[2] = 0;                         // padding

// cy_adpcm.c:100-101 - encoder writes DVI header at offset 3
dvi_adpcm_encode(ip_samples, AUDIO_FRAME_SIZE*2, op_frame+3,
                 &adpcm_pkt_len, (void *)&g_adpcm_state, 1);
// header_flag=1 → encoder prepends 3-byte DVI state header
```

The DVI header (`adpcm.c:120-124`):

```c
typedef struct __attribute__((__packed__)) {
    int16_t valpred;    // predictor, byte-swapped to big-endian
    uint8_t index;      // step table index (0-88)
} dvi_adpcm_state_t;   // 3 bytes
```

### The correct frame format

```
┌──────────────┬───────┬──────────────┬───────────┬──────────────────┐
│ SeqID (BE)   │ 0x00  │ Predictor BE │ StepIndex │ 128B ADPCM data  │
│ 2 bytes      │ 1B    │ 2 bytes      │ 1 byte    │ high nibble first│
├──────────────┴───────┼──────────────┴───────────┼──────────────────┤
│   App header (3B)    │    DVI header (3B)       │  256 samples     │
└──────────────────────┴──────────────────────────┴──────────────────┘
```

The header is **6 bytes** (3 app + 3 DVI), not 2 or 4 as we'd been trying.
Each frame is independently decodable - the decoder resets both predictor and
step_index from the DVI header.

### Why our earlier attempts failed

We had been treating bytes 3-5 as ADPCM data (in strategies A/B) or as a
different kind of header (strategies C/H/I). None of our guesses matched the
actual 3-byte DVI preamble structure.

When we finally parsed the header correctly:

```
Frame  0: pred=     0  step_idx=  0  → OK (silence)
Frame  2: pred=    -1  step_idx= 14  → OK (valid, small)
Frame  3: pred=  -158  step_idx= 35  → OK (valid, reasonable speech)
Frame 14: pred=    28  step_idx= 12  → OK (valid)
```

**100% of step_index values across all 95 frames were in the valid range 0-88.**
The decoded audio was clean, with peak values of ±6000 (not ±32768) and RMS of
236 (not 15972).

### Post-processing

The raw decoded audio from the remote's electret microphone is quiet and has
minor artifacts:

1. **Click removal** - single-sample spikes where both neighbors disagree are
   replaced with interpolated values
2. **Low-pass filter** - 3-tap triangle filter `[0.25, 0.5, 0.25]` removes
   high-frequency quantization noise
3. **RMS normalization** - target RMS of ~10,000 with spike-resistant gain
   calculation (95th percentile clipping threshold)

## Summary of Findings

### Protocol Stack

```
┌─────────────────────────────┐
│  ATVV (ab5e0001)            │  Voice-specific GATT service
│  - TX: commands             │
│  - RX: audio notifications  │
│  - CTL: control signals     │
├─────────────────────────────┤
│  HoGP (00001812)            │  Standard HID over GATT
│  - Keyboard/Mouse/Consumer  │
│  - Report ID 30 (unused)    │
├─────────────────────────────┤
│  BLE (bonded, encrypted)    │  AES-128-CCM
└─────────────────────────────┘
```

### Bugs Fixed During Reverse Engineering

| # | Bug                           | Symptom                                | Fix                                                                   |
| - | ----------------------------- | -------------------------------------- | --------------------------------------------------------------------- |
| 1 | Wrong BLE adapter             | `hci0` vs `hci1`                       | Check `busctl tree org.bluez` for actual adapter paths                |
| 2 | Wrong D-Bus interface         | No notifications received              | `org.freedesktop.DBus.Properties` (not `dbus.freedesktop.Properties`) |
| 3 | MIC_OPEN byte order           | Remote rejects with `0x0C 0x0F 0x01`   | Big-endian codec: `0x0C 0x00 0x01`                                    |
| 4 | Wrong header size             | Distorted audio (step index explosion) | 6-byte header: 3B app + 3B DVI                                        |
| 5 | Missing per-frame state reset | Decoder divergence, clipping           | Reset predictor + step_index from DVI header each frame               |
| 6 | Sample rate                   | Audio plays too fast                   | 8kHz (not 16kHz; GET_CAPS_RESP confirms)                              |

### Key Technical Details

| Parameter         | Value                                   |
| ----------------- | --------------------------------------- |
| GATT Service UUID | `ab5e0001-5a21-4f05-bc7d-af01f617b664`  |
| Codec             | IMA/DVI ADPCM, 8kHz, 16-bit, mono       |
| Compression       | 4:1 (4 bits per sample)                 |
| Frame size        | 134 bytes                               |
| Samples per frame | 257 (1 predictor + 256 decoded)         |
| Frame duration    | ~32ms                                   |
| Frame rate        | ~30.8 fps                               |
| BLE MTU           | 140 (full frame in single notification) |
| Nibble packing    | High nibble first within each byte      |
| Multi-byte fields | Big-endian throughout                   |
| Protocol version  | 0.4                                     |

## Voice Button Hold Detection

The G20S Pro manual instructs users to "press and hold" the voice button to
activate the microphone and release to deactivate. We investigated whether
hold/release state is detectable on any BLE channel.

### Observed behavior

The voice button fires an **instant press+release on every press**, regardless
of physical hold duration. This was confirmed at three levels:

**1. Linux input events (`evtest`):**

```
time 1774352650.499390, EV_KEY, KEY_VOICECOMMAND (582), value 1  (press)
time 1774352650.499415, EV_KEY, KEY_VOICECOMMAND (582), value 0  (release)
```

Press and release arrive in the same BLE notification (~25µs apart). Holding
the physical button produces no additional events - no repeat, no separate
release on button-up. This is Consumer Control usage `0x00CF` ("Voice Command")
from HID Report ID 3.

**2. ATVV CTL characteristic (`ab5e0004`):**

Each button press sends a single `START_SEARCH` (`0x08`). No corresponding
"stop" signal arrives on button release. When already streaming, some remotes
send `AUDIO_STOP` (`0x00`) on release - the G20S Pro does not; it sends another
`START_SEARCH` instead.

**3. Vendor service 0xFFF0 characteristics:**

The device exposes a vendor service (`0xFFF0`) with four characteristics:

| Handle | UUID | Flags                                |
| ------ | ---- | ------------------------------------ |
| 004c   | FFF1 | read, write-without-response, notify |
| 004f   | FFF2 | read, write-without-response, notify |
| 0052   | FFF3 | read, write                          |
| 0055   | FFF4 | read, write-without-response, notify |

We subscribed to notifications on all three notify-capable vendor
characteristics (FFF1, FFF2, FFF4) simultaneously with ATVV CTL and pressed
the voice button multiple times:

```
1774353026.129 [ATVV-CTL:char0060] 08 ([8])    ← START_SEARCH
1774353030.168 [ATVV-CTL:char0060] 08 ([8])    ← START_SEARCH
(no events on vendor characteristics)
```

**No data appeared on any vendor characteristic during voice button interaction.**

### 2.4GHz mode comparison

The same instant press+release behavior was observed in 2.4GHz USB dongle mode
using `xev`/`wev`, confirming this is a **firmware-level behavior** - not a
BLE-specific limitation or something that could be worked around by listening
to a different BLE channel.

### Conclusion

The G20S Pro firmware fires a single instantaneous press+release event for the
voice button on every press. No BLE channel (ATVV CTL, HID reports, or vendor
service 0xFFF0) exposes hold/release state. The manual's "hold to talk"
instruction likely describes the expected UX on Android TV, where the host
manages session duration via speech detection, not button hold state.

**Implication for atvvoice:** Toggle mode (press to start, press to stop) is the
correct default for this remote. A configurable `--mode hold` option exists for
remotes that do send `AUDIO_STOP` on button release, per the ATVV spec.

## Tools Used

- `bluetoothctl` - BLE pairing, GATT exploration
- `btmon` - raw BLE/HCI traffic capture
- `busctl` - D-Bus introspection of BlueZ objects
- `hidrd-convert` - HID report descriptor parsing
- `xxd` - raw binary inspection
- Python + `dbus-python` + `pygobject3` - prototype ATVV client
- Audacity - waveform and spectrogram analysis

## Scripts

Python tools used during the research, located in `scripts/`:

- **`atvv-capture.py`** - Working voice capture prototype. Implements the full
  ATVV handshake, decodes ADPCM, applies post-processing, saves WAV files.
- **`decode-test.py`** - Multi-strategy ADPCM decoder comparison. Tests
  combinations of header sizes, nibble orders, and state reset modes to find
  the correct frame format. Can re-decode saved raw frames offline.

```bash
nix shell nixpkgs#python3 nixpkgs#python3Packages.dbus-python nixpkgs#python3Packages.pygobject3 \
  --command python3 docs/research/scripts/atvv-capture.py
```

## References

[^1]: `hid-atv-remote.c` - Android kernel HID driver for ADT-1/Nexus Player
    remotes (VoHoGP method, not applicable to ATVV).
    https://android.googlesource.com/kernel/x86_64/+/f2aa02f9e6f019f7cee08b6b06d4636b210ffe36/drivers/hid/hid-atv-remote.c

[^2]: TI Voice over HID over GATT Profile documentation.
    https://software-dl.ti.com/lprf/simplelink_cc2640r2_latest/docs/blestack/ble_user_guide/html/voice/ble_voice.html

[^3]: BlueZ issue #1086 - "Google TV Remote Controls built-in mic is not
    detected". Contains protocol research and spec references.
    https://github.com/bluez/bluez/issues/1086

[^4]: CSDN blog translation of Google Voice over BLE spec - command table,
    characteristic UUIDs, codec values.
    https://blog.csdn.net/Weizhen_Huang/article/details/109251338

[^5]: Infineon CYW20829 Voice Remote reference firmware - source of truth for
    frame format, ADPCM encoding, and GATT notification structure.
    https://github.com/Infineon/mtb-example-btstack-freertos-cyw20829-voice-remote

[^6]: Google Voice over BLE spec v1.0.
    https://web.archive.org/web/20260324183034/https://wangefan.github.io/linux_kernel_driver/resources/Google_Voice_over_BLE_spec_v1.0.pdf

[^7]: Nordic DevZone discussion on Android TV voice input with Smart Remote 3.
    https://devzone.nordicsemi.com/f/nordic-q-a/25002/android-tv-voice-input-with-smart-remote-3

[^8]: CLUES BLE UUID database - identifies `ab5e0002` as Ohsung Electronics
    (Google Reference Design manufacturer).
    https://github.com/darkmentorllc/CLUES_Schema
