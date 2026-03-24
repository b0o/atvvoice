#!/usr/bin/env python3
"""Try multiple ADPCM decode strategies on raw G20S audio frames.

Usage:
  1. First, capture raw frames using g20s-atvv-test.py (modified to save raw)
  2. Then run: python3 docs/research/scripts/decode-test.py /path/to/raw_frames.bin

Or run directly to capture + decode:
  nix shell nixpkgs#python3 nixpkgs#python3Packages.dbus-python nixpkgs#python3Packages.pygobject3 \
    --command python3 docs/research/scripts/decode-test.py
"""

import struct
import wave
import os
import sys

OUTPUT_DIR = "/tmp"
SAMPLE_RATE = 8000

# IMA ADPCM tables
STEP_TABLE = [
    7,
    8,
    9,
    10,
    11,
    12,
    13,
    14,
    16,
    17,
    19,
    21,
    23,
    25,
    28,
    31,
    34,
    37,
    41,
    45,
    50,
    55,
    60,
    66,
    73,
    80,
    88,
    97,
    107,
    118,
    130,
    143,
    157,
    173,
    190,
    209,
    230,
    253,
    279,
    307,
    337,
    371,
    408,
    449,
    494,
    544,
    598,
    658,
    724,
    796,
    876,
    963,
    1060,
    1166,
    1282,
    1411,
    1552,
    1707,
    1878,
    2066,
    2272,
    2499,
    2749,
    3024,
    3327,
    3660,
    4026,
    4428,
    4871,
    5358,
    5894,
    6484,
    7132,
    7845,
    8630,
    9493,
    10442,
    11487,
    12635,
    13899,
    15289,
    16818,
    18500,
    20350,
    22385,
    24623,
    27086,
    29794,
    32767,
]
INDEX_TABLE = [-1, -1, -1, -1, 2, 4, 6, 8]


class ADPCMDecoder:
    def __init__(self, predictor=0, step_index=0):
        self.predictor = predictor
        self.step_index = step_index

    def decode_nibble(self, nibble):
        step = STEP_TABLE[self.step_index]
        diff = step >> 3
        if nibble & 1:
            diff += step >> 2
        if nibble & 2:
            diff += step >> 1
        if nibble & 4:
            diff += step
        if nibble & 8:
            self.predictor -= diff
        else:
            self.predictor += diff
        self.predictor = max(-32768, min(32767, self.predictor))
        self.step_index += INDEX_TABLE[nibble & 7]
        self.step_index = max(0, min(88, self.step_index))
        return self.predictor


def decode_stream(frames, strategy):
    """Decode frames with a given strategy. Returns PCM samples."""
    dec = ADPCMDecoder()
    all_samples = []

    for frame_idx, frame in enumerate(frames):
        if strategy == "A":
            # 2-byte seq header, rest is ADPCM, high nibble first, continuous state
            data_start = 2
            high_first = True
        elif strategy == "B":
            # 2-byte seq header, rest is ADPCM, LOW nibble first, continuous state
            data_start = 2
            high_first = False
        elif strategy == "C":
            # 2-byte seq + 2-byte predictor (BE), ADPCM from byte 4, high nibble first
            # Reset predictor per frame, step_index carries over
            data_start = 4
            high_first = True
            if len(frame) >= 4:
                dec.predictor = struct.unpack(">h", frame[2:4])[0]
        elif strategy == "D":
            # 2-byte seq + 2-byte predictor (BE), ADPCM from byte 4, LOW nibble first
            data_start = 4
            high_first = False
            if len(frame) >= 4:
                dec.predictor = struct.unpack(">h", frame[2:4])[0]
        elif strategy == "E":
            # First frame: 2-byte seq + 2-byte predictor (BE) + 1-byte step_index
            # Subsequent: 2-byte seq, rest ADPCM, continuous state
            # High nibble first
            high_first = True
            if frame_idx == 0 and len(frame) >= 5:
                dec.predictor = struct.unpack(">h", frame[2:4])[0]
                dec.step_index = min(88, frame[4])
                data_start = 5
                all_samples.append(dec.predictor)
            else:
                data_start = 2
        elif strategy == "F":
            # Same as E but LOW nibble first
            high_first = False
            if frame_idx == 0 and len(frame) >= 5:
                dec.predictor = struct.unpack(">h", frame[2:4])[0]
                dec.step_index = min(88, frame[4])
                data_start = 5
                all_samples.append(dec.predictor)
            else:
                data_start = 2
        elif strategy == "G":
            # 2-byte seq, rest ADPCM, high nibble first
            # Reset BOTH predictor and step_index per frame to 0
            data_start = 2
            high_first = True
            dec.predictor = 0
            dec.step_index = 0
        elif strategy == "H":
            # 4-byte header (seq + predictor LE), high nibble first, continuous step
            data_start = 4
            high_first = True
            if len(frame) >= 4:
                dec.predictor = struct.unpack("<h", frame[2:4])[0]
        elif strategy == "I":
            # 4-byte header (skip bytes 2-3), high nibble first, CONTINUOUS state
            data_start = 4
            high_first = True
        elif strategy == "J":
            # 4-byte header (skip bytes 2-3), LOW nibble first, CONTINUOUS state
            data_start = 4
            high_first = False
        elif strategy == "K":
            # 4-byte header, high nibble first, reset both pred+step to 0 per frame
            data_start = 4
            high_first = True
            dec.predictor = 0
            dec.step_index = 0
        elif strategy == "L":
            # 4-byte header, low nibble first, reset both pred+step to 0 per frame
            data_start = 4
            high_first = False
            dec.predictor = 0
            dec.step_index = 0
        else:
            raise ValueError(f"Unknown strategy: {strategy}")

        for i in range(data_start, len(frame)):
            byte = frame[i]
            if high_first:
                all_samples.append(dec.decode_nibble((byte >> 4) & 0x0F))
                all_samples.append(dec.decode_nibble(byte & 0x0F))
            else:
                all_samples.append(dec.decode_nibble(byte & 0x0F))
                all_samples.append(dec.decode_nibble((byte >> 4) & 0x0F))

    return all_samples


def save_wav(filename, samples):
    with wave.open(filename, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(SAMPLE_RATE)
        pcm = bytearray()
        for s in samples:
            pcm.extend(struct.pack("<h", s))
        wf.writeframes(pcm)


def capture_and_decode():
    """Capture raw frames via BLE, then decode with all strategies."""
    import dbus
    import dbus.mainloop.glib
    from gi.repository import GLib

    # Update these paths for your device (see atvv-capture.py header)
    DEVICE_PATH = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
    CHAR_TX = DEVICE_PATH + "/service005a/char005b"
    CHAR_RX = DEVICE_PATH + "/service005a/char005d"
    CHAR_CTL = DEVICE_PATH + "/service005a/char0060"
    BLUEZ = "org.bluez"
    CHAR_IFACE = "org.bluez.GattCharacteristic1"
    PROP_IFACE = "org.freedesktop.DBus.Properties"

    CMD_GET_CAPS = [0x0A, 0x00, 0x04, 0x00, 0x01]
    CMD_MIC_OPEN = [0x0C, 0x00, 0x01]
    CMD_MIC_CLOSE = [0x0D]

    raw_frames = []
    recording = False

    def on_props(interface, changed, invalidated, path=None):
        nonlocal recording, raw_frames
        if interface != CHAR_IFACE or "Value" not in changed:
            return
        value = bytes(changed["Value"])

        if path and path.endswith("char0060"):
            if len(value) == 0:
                return
            cmd = value[0]
            if cmd == 0x04:
                print("[CTL] AUDIO_START")
                recording = True
                raw_frames = []
            elif cmd == 0x00:
                print(f"[CTL] AUDIO_STOP ({len(raw_frames)} frames captured)")
                recording = False
                if raw_frames:
                    process_frames(raw_frames)
                char = bus.get_object(BLUEZ, CHAR_TX)
                char.WriteValue(
                    dbus.Array(CMD_MIC_CLOSE, signature="y"),
                    dbus.Dictionary({}, signature="sv"),
                    dbus_interface=CHAR_IFACE,
                )
            elif cmd == 0x08:
                if recording and raw_frames:
                    print(
                        f"[CTL] START_SEARCH (stopping previous: {len(raw_frames)} frames)"
                    )
                    recording = False
                    process_frames(raw_frames)
                    raw_frames = []
                else:
                    print("[CTL] START_SEARCH - sending MIC_OPEN")
                char = bus.get_object(BLUEZ, CHAR_TX)
                char.WriteValue(
                    dbus.Array(CMD_MIC_OPEN, signature="y"),
                    dbus.Dictionary({}, signature="sv"),
                    dbus_interface=CHAR_IFACE,
                )
            elif cmd == 0x0B:
                print(f"[CTL] GET_CAPS_RESP: {value.hex()}")
            else:
                print(f"[CTL] cmd=0x{cmd:02x} data={value.hex()}")

        elif path and path.endswith("char005d"):
            if recording:
                raw_frames.append(value)
                if len(raw_frames) % 50 == 0:
                    print(f"  ...{len(raw_frames)} frames")

    def process_frames(frames):
        print(f"\nDecoding {len(frames)} frames with 8 strategies...")
        # Save raw frames for offline analysis
        raw_path = os.path.join(OUTPUT_DIR, "g20s_raw_frames.bin")
        with open(raw_path, "wb") as f:
            for frame in frames:
                f.write(struct.pack(">H", len(frame)))
                f.write(frame)
        print(f"  Raw frames saved to {raw_path}")

        strategies = ["H", "I", "J", "K", "L"]
        labels = {
            "H": "4B-hdr, pred-LE-reset, high-first",
            "I": "4B-hdr, skip, high-first, CONTINUOUS",
            "J": "4B-hdr, skip, LOW-first, CONTINUOUS",
            "K": "4B-hdr, high-first, full-reset-per-frame",
            "L": "4B-hdr, LOW-first, full-reset-per-frame",
        }

        for s in strategies:
            samples = decode_stream(frames, s)
            filename = os.path.join(OUTPUT_DIR, f"g20s_decode_{s}.wav")
            save_wav(filename, samples)
            duration = len(samples) / SAMPLE_RATE
            print(f"  [{s}] {labels[s]}: {filename} ({duration:.1f}s)")

        print("\nPlay each and find the cleanest:")
        print(
            "  for f in /tmp/g20s_decode_{H,I,J,K,L}.wav; do echo $f; pw-play $f; sleep 1; done"
        )

    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()

    bus.add_signal_receiver(
        lambda iface, changed, inv, path: on_props(iface, changed, inv, path=path),
        signal_name="PropertiesChanged",
        dbus_interface=PROP_IFACE,
        bus_name=BLUEZ,
        path_keyword="path",
    )

    for char_path in [CHAR_CTL, CHAR_RX]:
        try:
            char = bus.get_object(BLUEZ, char_path)
            char.StartNotify(dbus_interface=CHAR_IFACE)
        except Exception:
            pass

    import time

    time.sleep(0.5)
    char = bus.get_object(BLUEZ, CHAR_TX)
    char.WriteValue(
        dbus.Array(CMD_GET_CAPS, signature="y"),
        dbus.Dictionary({}, signature="sv"),
        dbus_interface=CHAR_IFACE,
    )
    print("Ready. Press and hold mic, speak, release.")
    print("Press Ctrl+C to exit.\n")

    loop = GLib.MainLoop()
    try:
        loop.run()
    except KeyboardInterrupt:
        if raw_frames:
            print(f"\nProcessing {len(raw_frames)} captured frames...")
            process_frames(raw_frames)
        print("\nDone.")


if __name__ == "__main__":
    if len(sys.argv) > 1:
        # Offline mode: decode from saved raw frames
        raw_path = sys.argv[1]
        frames = []
        with open(raw_path, "rb") as f:
            while True:
                hdr = f.read(2)
                if len(hdr) < 2:
                    break
                length = struct.unpack(">H", hdr)[0]
                frame = f.read(length)
                if len(frame) < length:
                    break
                frames.append(frame)
        print(f"Loaded {len(frames)} frames from {raw_path}")

        strategies = ["H", "I", "J", "K", "L"]
        labels = {
            "H": "4B-hdr, pred-LE-reset, high-first",
            "I": "4B-hdr, skip, high-first, CONTINUOUS",
            "J": "4B-hdr, skip, LOW-first, CONTINUOUS",
            "K": "4B-hdr, high-first, full-reset-per-frame",
            "L": "4B-hdr, LOW-first, full-reset-per-frame",
        }

        for s in strategies:
            samples = decode_stream(frames, s)
            filename = os.path.join(OUTPUT_DIR, f"g20s_decode_{s}.wav")
            save_wav(filename, samples)
            duration = len(samples) / SAMPLE_RATE
            print(f"  [{s}] {labels[s]}: {filename} ({duration:.1f}s)")
    else:
        capture_and_decode()
