/// IMA/DVI ADPCM step size table (89 entries).
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37,
    41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173,
    190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658,
    724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894,
    6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289,
    16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// IMA/DVI ADPCM index adjustment table.
const INDEX_TABLE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// ATVV audio frame size in bytes.
pub const FRAME_SIZE: usize = 134;

/// Number of PCM samples decoded from one frame.
/// 1 (predictor) + 256 (128 bytes × 2 nibbles).
pub const SAMPLES_PER_FRAME: usize = 257;

/// Decode one 134-byte ATVV audio frame into PCM samples.
///
/// Frame layout:
///   Bytes 0-1: Sequence ID (big-endian) - returned separately
///   Byte 2:    0x00 (padding)
///   Bytes 3-4: DVI predictor (big-endian, signed 16-bit)
///   Byte 5:    DVI step table index (0-88)
///   Bytes 6-133: 128 bytes IMA ADPCM (high nibble first)
///
/// Returns (sequence_number, pcm_samples).
pub fn decode_frame(frame: &[u8; FRAME_SIZE]) -> (u16, [i16; SAMPLES_PER_FRAME]) {
    let seq = u16::from_be_bytes([frame[0], frame[1]]);
    let mut predictor = i16::from_be_bytes([frame[3], frame[4]]) as i32;
    let mut step_index = frame[5].min(88) as i32;

    let mut samples = [0i16; SAMPLES_PER_FRAME];
    samples[0] = predictor as i16;

    let mut sample_idx = 1;
    for &byte in &frame[6..] {
        // High nibble first
        for nibble in [(byte >> 4) & 0x0F, byte & 0x0F] {
            let step = STEP_TABLE[step_index as usize];
            let mut diff = step >> 3;
            if nibble & 1 != 0 {
                diff += step >> 2;
            }
            if nibble & 2 != 0 {
                diff += step >> 1;
            }
            if nibble & 4 != 0 {
                diff += step;
            }
            if nibble & 8 != 0 {
                predictor -= diff;
            } else {
                predictor += diff;
            }
            predictor = predictor.clamp(-32768, 32767);

            step_index += INDEX_TABLE[(nibble & 7) as usize];
            step_index = step_index.clamp(0, 88);

            samples[sample_idx] = predictor as i16;
            sample_idx += 1;
        }
    }

    (seq, samples)
}

/// Minimum amplitude jump to consider a sample a click spike.
/// Empirically tuned for the G20S Pro electret microphone output.
const DECLIP_THRESHOLD: i32 = 1000;

/// Remove single-sample click spikes by interpolation.
pub fn declip(samples: &mut [i16]) {
    for i in 1..samples.len().saturating_sub(1) {
        let prev = samples[i - 1] as i32;
        let cur = samples[i] as i32;
        let nxt = samples[i + 1] as i32;
        let dp = (cur - prev).abs();
        let dn = (cur - nxt).abs();
        let neighbor_diff = (nxt - prev).abs();
        if dp > DECLIP_THRESHOLD && dn > DECLIP_THRESHOLD && dp.min(dn) > neighbor_diff * 2 {
            samples[i] = ((prev + nxt) / 2) as i16;
        }
    }
}

/// Apply 3-tap triangle low-pass filter \[0.25, 0.5, 0.25\].
///
/// Note: first and last samples pass through unfiltered. This is intentional
/// for per-frame processing - the predictor at sample[0] provides continuity.
pub fn lowpass(samples: &mut [i16]) {
    if samples.len() < 3 {
        return;
    }
    let mut prev = samples[0] as i32;
    for i in 1..samples.len() - 1 {
        let cur = samples[i] as i32;
        let nxt = samples[i + 1] as i32;
        let filtered = (prev + 2 * cur + nxt) >> 2;
        samples[i] = filtered as i16;
        prev = cur; // use unfiltered value for next iteration
    }
}

/// Apply fixed gain (in dB) with clamping.
pub fn apply_gain(samples: &mut [i16], gain_db: f32) {
    let gain = 10f32.powf(gain_db / 20.0);
    for s in samples.iter_mut() {
        let amplified = (*s as f32 * gain).round() as i32;
        *s = amplified.clamp(-32768, 32767) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal IMA ADPCM encoder for round-trip testing.
    /// Returns a 134-byte frame: 3B app header + 3B DVI header + 128B ADPCM data.
    fn encode_samples(samples: &[i16], seq: u16) -> [u8; FRAME_SIZE] {
        assert!(samples.len() >= 257, "need at least 257 samples");

        let mut frame = [0u8; FRAME_SIZE];

        // App header: sequence ID (big-endian) + padding byte
        let seq_bytes = seq.to_be_bytes();
        frame[0] = seq_bytes[0];
        frame[1] = seq_bytes[1];
        frame[2] = 0x00;

        // DVI header: predictor (big-endian i16) + step_index
        let predictor = samples[0];
        let pred_bytes = predictor.to_be_bytes();
        frame[3] = pred_bytes[0];
        frame[4] = pred_bytes[1];
        // Pick an initial step_index that matches the signal amplitude.
        // Find the step size closest to the first sample-to-sample difference.
        let initial_diff = (samples[1] as i32 - samples[0] as i32).unsigned_abs() as i32;
        let mut init_step_index: i32 = 0;
        for (i, &step) in STEP_TABLE.iter().enumerate() {
            if step <= initial_diff {
                init_step_index = i as i32;
            } else {
                break;
            }
        }
        frame[5] = init_step_index as u8;

        // Encode 256 samples into 128 ADPCM bytes (high nibble first)
        let mut pred = predictor as i32;
        let mut step_index: i32 = init_step_index;

        let mut byte_idx = 6;
        let mut sample_idx = 1;
        while byte_idx < FRAME_SIZE {
            let mut packed_byte: u8 = 0;

            for shift in [4u8, 0u8] {
                let sample = samples[sample_idx] as i32;
                sample_idx += 1;

                let step = STEP_TABLE[step_index as usize];
                let diff = sample - pred;

                let mut nibble: u8 = 0;
                let mut predicted_diff = step >> 3;

                if diff < 0 {
                    nibble |= 8;
                }
                let abs_diff = diff.unsigned_abs() as i32;

                if abs_diff >= step {
                    nibble |= 4;
                    predicted_diff += step;
                }
                if abs_diff >= (step >> 1) + (if nibble & 4 != 0 { step } else { 0 }) {
                    nibble |= 2;
                    predicted_diff += step >> 1;
                }
                if abs_diff
                    >= (step >> 2)
                        + (if nibble & 4 != 0 { step } else { 0 })
                        + (if nibble & 2 != 0 { step >> 1 } else { 0 })
                {
                    nibble |= 1;
                    predicted_diff += step >> 2;
                }

                if nibble & 8 != 0 {
                    pred -= predicted_diff;
                } else {
                    pred += predicted_diff;
                }
                pred = pred.clamp(-32768, 32767);

                step_index += INDEX_TABLE[(nibble & 7) as usize];
                step_index = step_index.clamp(0, 88);

                packed_byte |= nibble << shift;
            }

            frame[byte_idx] = packed_byte;
            byte_idx += 1;
        }

        frame
    }

    #[test]
    fn test_round_trip_sine_wave() {
        // Generate 440Hz sine wave at 8kHz sample rate, 257 samples
        let mut samples = [0i16; 257];
        for i in 0..257 {
            let t = i as f64 / 8000.0;
            samples[i] = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
        }

        let frame = encode_samples(&samples, 42);
        let (seq, decoded) = decode_frame(&frame);

        assert_eq!(seq, 42);
        assert_eq!(decoded.len(), SAMPLES_PER_FRAME);

        // Calculate SNR: signal power / noise power
        let mut signal_power: f64 = 0.0;
        let mut noise_power: f64 = 0.0;
        for i in 0..257 {
            let s = samples[i] as f64;
            let d = decoded[i] as f64;
            signal_power += s * s;
            noise_power += (s - d) * (s - d);
        }

        let snr_db = 10.0 * (signal_power / noise_power).log10();
        assert!(
            snr_db > 20.0,
            "SNR should be > 20dB, got {snr_db:.1}dB"
        );
    }

    #[test]
    fn test_silent_frame() {
        // Frame with predictor=0, step_index=0, all-zero ADPCM bytes
        let mut frame = [0u8; FRAME_SIZE];
        frame[0] = 0x00; // seq high
        frame[1] = 0x01; // seq low = 1
        frame[2] = 0x00; // padding
        frame[3] = 0x00; // predictor high
        frame[4] = 0x00; // predictor low = 0
        frame[5] = 0x00; // step_index = 0
        // bytes 6..133 already zero (all-zero nibbles)

        let (seq, samples) = decode_frame(&frame);

        assert_eq!(seq, 1);
        // All-zero nibbles with predictor=0 and step_index=0:
        // nibble 0x0 -> diff = step>>3 = 7>>3 = 0 (integer division), predictor += 0
        // Actually step>>3 = 0 for step=7, so predictor stays near 0
        // With step_index decreasing by 1 (INDEX_TABLE[0]=-1), clamped to 0
        // All samples should be very close to 0
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s.abs() <= 5,
                "Sample {i} should be near-silent, got {s}"
            );
        }
    }

    #[test]
    fn test_header_parsing() {
        let mut frame = [0u8; FRAME_SIZE];

        // Sequence: 0x1234
        frame[0] = 0x12;
        frame[1] = 0x34;

        // Padding
        frame[2] = 0x00;

        // Predictor: -500 in big-endian signed i16
        let pred_bytes = (-500i16).to_be_bytes();
        frame[3] = pred_bytes[0];
        frame[4] = pred_bytes[1];

        // Step index: 44
        frame[5] = 44;

        let (seq, samples) = decode_frame(&frame);

        assert_eq!(seq, 0x1234);
        // First sample should be the predictor value
        assert_eq!(samples[0], -500);
    }

    #[test]
    fn test_known_vector() {
        // Construct a frame with known DVI header and hand-computed nibbles
        let mut frame = [0u8; FRAME_SIZE];

        // Sequence: 0x0007
        frame[0] = 0x00;
        frame[1] = 0x07;
        frame[2] = 0x00;

        // Predictor: 1000 (big-endian)
        let pred_bytes = (1000i16).to_be_bytes();
        frame[3] = pred_bytes[0];
        frame[4] = pred_bytes[1];

        // Step index: 20
        frame[5] = 20;

        // Hand-compute first few nibbles:
        // step_index=20, step=STEP_TABLE[20]=50
        //
        // First byte, high nibble = 0x3 (bits: 0011)
        //   diff = 50 >> 3 = 6
        //   bit0 set (0x1): diff += 50 >> 2 = 12 -> diff = 18
        //   bit1 set (0x2): diff += 50 >> 1 = 25 -> diff = 43
        //   bit2 not set, bit3 not set (positive)
        //   predictor = 1000 + 43 = 1043
        //   step_index = 20 + INDEX_TABLE[3] = 20 + (-1) = 19
        //
        // First byte, low nibble = 0x5 (bits: 0101)
        //   step_index=19, step=STEP_TABLE[19]=45
        //   diff = 45 >> 3 = 5
        //   bit0 set (0x1): diff += 45 >> 2 = 11 -> diff = 16
        //   bit1 not set
        //   bit2 set (0x4): diff += 45 -> diff = 61
        //   bit3 not set (positive)
        //   predictor = 1043 + 61 = 1104
        //   step_index = 19 + INDEX_TABLE[5] = 19 + 4 = 23

        frame[6] = 0x35; // high nibble = 0x3, low nibble = 0x5

        // Second byte, high nibble = 0x9 (bits: 1001)
        //   step_index=23, step=STEP_TABLE[23]=66
        //   diff = 66 >> 3 = 8
        //   bit0 set (0x1): diff += 66 >> 2 = 16 -> diff = 24
        //   bit1 not set
        //   bit2 not set
        //   bit3 set (negative)
        //   predictor = 1104 - 24 = 1080
        //   step_index = 23 + INDEX_TABLE[1] = 23 + (-1) = 22

        frame[7] = 0x90; // high nibble = 0x9, low nibble = 0x0

        // Rest of the frame is zeros (won't affect first few samples)

        let (seq, samples) = decode_frame(&frame);

        assert_eq!(seq, 7);
        assert_eq!(samples[0], 1000);  // predictor
        assert_eq!(samples[1], 1043);  // after nibble 0x3
        assert_eq!(samples[2], 1104);  // after nibble 0x5
        assert_eq!(samples[3], 1080);  // after nibble 0x9
    }

    #[test]
    fn test_step_index_clamped() {
        // Verify step_index > 88 gets clamped to 88
        let mut frame = [0u8; FRAME_SIZE];
        frame[5] = 99; // step_index out of range, should be clamped to 88

        let (_seq, samples) = decode_frame(&frame);
        // Should not panic; first sample is predictor (0)
        assert_eq!(samples[0], 0);
    }

    // --- Post-processing tests ---

    #[test]
    fn test_declip_removes_spike() {
        // A single-sample spike surrounded by low values should be interpolated
        let mut samples: Vec<i16> = vec![0, 0, 5000, 0, 0];
        declip(&mut samples);
        assert_eq!(samples[2], 0, "Spike at index 2 should be interpolated to 0");
        // Neighbors should be unchanged
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1], 0);
        assert_eq!(samples[3], 0);
        assert_eq!(samples[4], 0);
    }

    #[test]
    fn test_declip_preserves_gradual_changes() {
        // Gradual ramp should not be modified
        let mut samples: Vec<i16> = vec![0, 100, 200, 300, 400];
        let original = samples.clone();
        declip(&mut samples);
        assert_eq!(samples, original, "Gradual ramp should not be modified");
    }

    #[test]
    fn test_declip_negative_spike() {
        // A negative spike should also be removed
        let mut samples: Vec<i16> = vec![100, 100, -5000, 100, 100];
        declip(&mut samples);
        assert_eq!(samples[2], 100, "Negative spike should be interpolated to 100");
    }

    #[test]
    fn test_lowpass_smooths_step() {
        // Step function should get smoothed at the transition
        let mut samples: Vec<i16> = vec![0, 0, 0, 100, 100, 100];
        lowpass(&mut samples);
        // At index 2 (transition): (0 + 2*0 + 100) / 4 = 25
        assert_eq!(samples[2], 25);
        // At index 3 (transition): (0 + 2*100 + 100) / 4 = 75
        assert_eq!(samples[3], 75);
        // Endpoints should be unchanged
        assert_eq!(samples[0], 0);
        assert_eq!(samples[5], 100);
    }

    #[test]
    fn test_lowpass_preserves_constant() {
        // Constant signal should pass through unchanged
        let mut samples: Vec<i16> = vec![500, 500, 500, 500, 500];
        lowpass(&mut samples);
        for &s in &samples {
            assert_eq!(s, 500);
        }
    }

    #[test]
    fn test_lowpass_short_input() {
        // Less than 3 samples should be unchanged
        let mut samples: Vec<i16> = vec![100, 200];
        let original = samples.clone();
        lowpass(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn test_apply_gain_20db() {
        // 20dB = 10x amplification
        let mut samples: Vec<i16> = vec![0, 100, -100, 1000, -1000];
        apply_gain(&mut samples, 20.0);
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1], 1000);
        assert_eq!(samples[2], -1000);
        assert_eq!(samples[3], 10000);
        assert_eq!(samples[4], -10000);
    }

    #[test]
    fn test_apply_gain_clamps() {
        // Large gain should clamp at i16 limits
        let mut samples: Vec<i16> = vec![10000, -10000];
        apply_gain(&mut samples, 20.0); // 10x would give ±100000, must clamp
        assert_eq!(samples[0], 32767);
        assert_eq!(samples[1], -32768); // i16::MIN is -32768
    }

    #[test]
    fn test_apply_gain_zero_db() {
        // 0dB = 1x, no change
        let mut samples: Vec<i16> = vec![100, -100, 32767, -32768];
        let original = samples.clone();
        apply_gain(&mut samples, 0.0);
        assert_eq!(samples, original);
    }

    #[test]
    fn test_apply_gain_negative_db() {
        // -20dB = 0.1x attenuation
        let mut samples: Vec<i16> = vec![10000, -10000];
        apply_gain(&mut samples, -20.0);
        assert_eq!(samples[0], 1000);
        assert_eq!(samples[1], -1000);
    }
}
