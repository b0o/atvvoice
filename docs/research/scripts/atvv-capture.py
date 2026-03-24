#!/usr/bin/env python3
"""G20S Pro ATVV Voice Capture — captures mic audio and saves to WAV.

Implements the Google Voice over BLE (ATVV) protocol with correct
DVI/IMA ADPCM decoding.

Frame format (134 bytes):
  Bytes 0-1:   Sequence ID (big-endian)
  Byte  2:     0x00 (padding)
  Bytes 3-4:   DVI predictor value (big-endian, signed 16-bit)
  Byte  5:     DVI step table index (0-88)
  Bytes 6-133: 128 bytes of IMA ADPCM nibbles (high nibble first)
               = 256 samples per frame

Audio: 8kHz, 16-bit mono, IMA/DVI ADPCM 4:1 compression

Usage:
  nix shell nixpkgs#python3 nixpkgs#python3Packages.dbus-python nixpkgs#python3Packages.pygobject3 \
    --command python3 docs/research/scripts/atvv-capture.py
"""

import dbus
import dbus.mainloop.glib
from gi.repository import GLib
import struct
import wave
import os
import time

# Update these paths for your device. Find them with:
#   busctl tree org.bluez | grep dev_
#   busctl tree org.bluez | grep service
DEVICE_PATH = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"
CHAR_TX = DEVICE_PATH + "/service005a/char005b"  # ab5e0002 - Write
CHAR_RX = DEVICE_PATH + "/service005a/char005d"  # ab5e0003 - Notify (audio)
CHAR_CTL = DEVICE_PATH + "/service005a/char0060"  # ab5e0004 - Notify (control)

BLUEZ = "org.bluez"
CHAR_IFACE = "org.bluez.GattCharacteristic1"
PROP_IFACE = "org.freedesktop.DBus.Properties"

CMD_GET_CAPS = [0x0A, 0x00, 0x04, 0x00, 0x01]
CMD_MIC_OPEN = [0x0C, 0x00, 0x01]
CMD_MIC_CLOSE = [0x0D]

SAMPLE_RATE = 8000
OUTPUT_DIR = "/tmp"

# --- IMA/DVI ADPCM tables ---
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


def decode_nibble(n, pred, idx):
    """Decode one 4-bit IMA ADPCM nibble."""
    step = STEP_TABLE[idx]
    d = step >> 3
    if n & 1:
        d += step >> 2
    if n & 2:
        d += step >> 1
    if n & 4:
        d += step
    if n & 8:
        pred -= d
    else:
        pred += d
    pred = max(-32768, min(32767, pred))
    idx = max(0, min(88, idx + INDEX_TABLE[n & 7]))
    return pred, idx


def decode_frame(frame_data):
    """Decode a 134-byte ATVV audio frame.

    Header: 3 bytes app (seq_hi, seq_lo, 0x00) + 3 bytes DVI (pred_be, index)
    Data: 128 bytes ADPCM = 256 samples, high nibble first.
    """
    if len(frame_data) < 6:
        return []

    pred = struct.unpack(">h", frame_data[3:5])[0]
    idx = min(88, frame_data[5])
    samples = [pred]

    for byte in frame_data[6:]:
        pred, idx = decode_nibble((byte >> 4) & 0xF, pred, idx)
        samples.append(pred)
        pred, idx = decode_nibble(byte & 0xF, pred, idx)
        samples.append(pred)

    return samples


class AudioCapture:
    def __init__(self):
        self.pcm_data = bytearray()
        self.frame_count = 0
        self.recording = False
        self.recording_num = 0

    def start_recording(self):
        self.recording = True
        self.pcm_data = bytearray()
        self.frame_count = 0

    def stop_recording(self):
        if not self.recording:
            return
        self.recording = False
        if len(self.pcm_data) == 0:
            print("[REC] No audio data to save")
            return

        # Post-process: declip, filter noise, normalize
        samples = []
        for i in range(0, len(self.pcm_data), 2):
            s = struct.unpack("<h", self.pcm_data[i : i + 2])[0]
            samples.append(s)

        # 1. Remove single-sample click spikes (interpolate)
        for i in range(1, len(samples) - 1):
            prev, cur, nxt = samples[i - 1], samples[i], samples[i + 1]
            dp = abs(cur - prev)
            dn = abs(cur - nxt)
            avg_neighbor = abs(nxt - prev)
            if dp > 1000 and dn > 1000 and min(dp, dn) > avg_neighbor * 2:
                samples[i] = (prev + nxt) // 2

        # 2. Simple low-pass filter (3-tap triangle: [0.25, 0.5, 0.25])
        #    Cuts high-frequency quantization noise
        filtered = [samples[0]]
        for i in range(1, len(samples) - 1):
            filtered.append((samples[i - 1] + 2 * samples[i] + samples[i + 1]) >> 2)
        filtered.append(samples[-1])
        samples = filtered

        # 3. Normalize based on RMS
        rms = (sum(s * s for s in samples) / len(samples)) ** 0.5 if samples else 1
        target_rms = 10000
        gain = min(target_rms / rms, 25.0) if rms > 0 else 1.0

        self.recording_num += 1
        filename = os.path.join(
            OUTPUT_DIR, f"g20s_recording_{self.recording_num:03d}.wav"
        )

        with wave.open(filename, "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(SAMPLE_RATE)
            for s in samples:
                amplified = max(-32768, min(32767, int(s * gain)))
                wf.writeframes(struct.pack("<h", amplified))

        duration = len(samples) / SAMPLE_RATE
        print(f"[REC] Saved {filename}")
        print(f"      {duration:.1f}s, {self.frame_count} frames, gain={gain:.1f}x")

    def on_audio_data(self, frame_data):
        self.frame_count += 1

        if self.recording:
            samples = decode_frame(frame_data)
            for s in samples:
                self.pcm_data.extend(struct.pack("<h", s))

        if self.frame_count <= 3:
            seq = (frame_data[0] << 8) | frame_data[1] if len(frame_data) >= 2 else -1
            pred = (
                struct.unpack(">h", frame_data[3:5])[0] if len(frame_data) >= 5 else 0
            )
            idx = frame_data[5] if len(frame_data) >= 6 else 0
            print(f"[AUDIO] frame {seq}: pred={pred} idx={idx} ({len(frame_data)}B)")
        elif self.frame_count == 4:
            print(f"[AUDIO] (streaming...)")
        elif self.frame_count % 100 == 0:
            print(f"[AUDIO] ...{self.frame_count} frames")


capture = AudioCapture()


def on_properties_changed(interface, changed, invalidated, path=None):
    if interface != CHAR_IFACE or "Value" not in changed:
        return

    value = bytes(changed["Value"])

    if path and path.endswith("char0060"):
        if len(value) == 0:
            return
        cmd = value[0]
        if cmd == 0x00:
            print(f"[CTL] AUDIO_STOP")
            capture.stop_recording()
            send_mic_close()
        elif cmd == 0x04:
            print(f"[CTL] AUDIO_START")
            capture.start_recording()
        elif cmd == 0x08:
            if capture.recording:
                capture.stop_recording()
            send_mic_open()
        elif cmd == 0x0B:
            print(f"[CTL] GET_CAPS_RESP: {value.hex()}")
        else:
            print(f"[CTL] cmd=0x{cmd:02x} data={value.hex()}")

    elif path and path.endswith("char005d"):
        capture.on_audio_data(value)


def send_mic_open():
    try:
        char = bus.get_object(BLUEZ, CHAR_TX)
        char.WriteValue(
            dbus.Array(CMD_MIC_OPEN, signature="y"),
            dbus.Dictionary({}, signature="sv"),
            dbus_interface=CHAR_IFACE,
        )
        print("[TX] MIC_OPEN")
    except Exception as e:
        print(f"[TX] MIC_OPEN failed: {e}")


def send_mic_close():
    try:
        char = bus.get_object(BLUEZ, CHAR_TX)
        char.WriteValue(
            dbus.Array(CMD_MIC_CLOSE, signature="y"),
            dbus.Dictionary({}, signature="sv"),
            dbus_interface=CHAR_IFACE,
        )
    except Exception:
        pass


def send_get_caps():
    try:
        char = bus.get_object(BLUEZ, CHAR_TX)
        char.WriteValue(
            dbus.Array(CMD_GET_CAPS, signature="y"),
            dbus.Dictionary({}, signature="sv"),
            dbus_interface=CHAR_IFACE,
        )
        print("[TX] GET_CAPS")
    except Exception as e:
        print(f"[TX] GET_CAPS failed: {e}")


def enable_notifications(char_path, label):
    try:
        char = bus.get_object(BLUEZ, char_path)
        char.StartNotify(dbus_interface=CHAR_IFACE)
        print(f"[OK] Notify on {label}")
    except dbus.exceptions.DBusException as e:
        if "Already notifying" in str(e) or "In Progress" in str(e):
            print(f"[OK] Already notifying on {label}")
        else:
            print(f"[ERR] {label}: {e}")


def main():
    global bus

    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()

    print("=" * 50)
    print("G20S Pro Voice Capture")
    print("=" * 50)

    bus.add_signal_receiver(
        lambda iface, changed, inv, path: on_properties_changed(
            iface, changed, inv, path=path
        ),
        signal_name="PropertiesChanged",
        dbus_interface=PROP_IFACE,
        bus_name=BLUEZ,
        path_keyword="path",
    )

    enable_notifications(CHAR_CTL, "CTL")
    enable_notifications(CHAR_RX, "RX")

    time.sleep(0.5)
    send_get_caps()

    print()
    print("Hold mic button to record, press again to stop & save.")
    print("Ctrl+C to exit.")
    print()

    loop = GLib.MainLoop()
    try:
        loop.run()
    except KeyboardInterrupt:
        capture.stop_recording()
        print("\nDone.")


if __name__ == "__main__":
    main()
