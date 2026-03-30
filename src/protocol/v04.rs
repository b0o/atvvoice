use anyhow::Result;

use super::types::*;
use super::Protocol;
use crate::adpcm::AdpcmDecoder;

// v0.4 CTL payload minimum lengths (including opcode byte).
const AUDIO_SYNC_MIN_LEN: usize = 3; // opcode + frame_num(2)
const CAPS_RESP_MIN_LEN: usize = 9; // opcode + version(2) + codecs(2) + frame_size(2) + bytes_per_char(2)
const MIC_OPEN_ERROR_MIN_LEN: usize = 3; // opcode + error_code(2)

/// v0.4 audio frame header: seq(2) + padding(1) + predictor(2) + step_index(1).
const FRAME_HEADER_SIZE: usize = 6;

/// ATVV v0.4 protocol implementation.
pub struct ProtocolV04 {
    pub selected_codec: Option<Codec>,
    pub audio_frame_size: AudioFrameSize,
    decoder: AdpcmDecoder,
}

impl Default for ProtocolV04 {
    fn default() -> Self {
        Self {
            selected_codec: None,
            audio_frame_size: AudioFrameSize::DEFAULT_V04,
            decoder: AdpcmDecoder::new(),
        }
    }
}

impl ProtocolV04 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Negotiate the best codec from the intersection of remote and our support.
    /// Prefers highest quality (16kHz > 8kHz).
    fn negotiate_codec(remote_codecs: Codecs) -> Result<Codec> {
        // Our supported codecs (both ADPCM variants).
        let ours = Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ;
        let common = remote_codecs & ours;
        if common.contains(Codecs::ADPCM_16KHZ) {
            Ok(Codec::Adpcm16kHz)
        } else if common.contains(Codecs::ADPCM_8KHZ) {
            Ok(Codec::Adpcm8kHz)
        } else {
            anyhow::bail!(
                "no common codec: remote supports {:?}, we support {:?}",
                remote_codecs,
                ours
            )
        }
    }
}

impl Protocol for ProtocolV04 {
    fn version(&self) -> ProtocolVersion {
        ProtocolVersion::V0_4
    }

    fn get_caps_cmd(&self) -> Vec<u8> {
        let ver = ProtocolVersion::V0_4.wire_value().to_be_bytes();
        let codecs = (Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ).bits();
        vec![u8::from(TxOpcode::GetCaps), ver[0], ver[1], 0x00, codecs]
    }

    fn mic_open_cmd(&self) -> Vec<u8> {
        let codec_val = u8::from(self.selected_codec.unwrap_or(Codec::Adpcm8kHz));
        vec![u8::from(TxOpcode::MicOpen), 0x00, codec_val]
    }

    fn mic_close_cmd(&self, _stream_id: StreamId) -> Vec<u8> {
        // v0.4: no stream_id
        vec![u8::from(TxOpcode::MicClose)]
    }

    fn keepalive_cmd(&self, _stream_id: StreamId) -> Vec<u8> {
        // v0.4: no MIC_EXTEND, fall back to MIC_OPEN
        self.mic_open_cmd()
    }

    fn parse_ctl(&self, data: &[u8]) -> CtlEvent {
        if data.is_empty() {
            return CtlEvent::Unknown(data.to_vec());
        }
        let opcode = match CtlOpcode::try_from(data[0]) {
            Ok(op) => op,
            Err(_) => return CtlEvent::Unknown(data.to_vec()),
        };
        match opcode {
            CtlOpcode::AudioStop => {
                // v0.4: no payload, synthesize default reason
                CtlEvent::AudioStop {
                    reason: AudioStopReason::MicClose,
                }
            }
            CtlOpcode::AudioStart => {
                // v0.4: no payload, synthesize defaults
                CtlEvent::AudioStart {
                    reason: AudioStartReason::MicOpen,
                    codec: self.selected_codec.unwrap_or(Codec::Adpcm8kHz),
                    stream_id: StreamId::MIC_OPEN,
                }
            }
            CtlOpcode::StartSearch => CtlEvent::StartSearch,
            CtlOpcode::AudioSync => {
                // v0.4: [opcode, frame_num_hi, frame_num_lo]
                if data.len() >= AUDIO_SYNC_MIN_LEN {
                    let seq = u16::from_be_bytes([data[1], data[2]]);
                    CtlEvent::AudioSync(AudioSyncData::FrameNum { seq })
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::CapsResp => {
                // v0.4: [opcode, version(2), codecs_supported(2), bytes_per_frame(2), bytes_per_characteristic(2)]
                if data.len() >= CAPS_RESP_MIN_LEN {
                    let version_wire = u16::from_be_bytes([data[1], data[2]]);
                    let version =
                        ProtocolVersion::from_wire(version_wire).unwrap_or(ProtocolVersion::V0_4);
                    // codecs_supported is 2 bytes in v0.4; only low byte has values
                    let codecs = Codecs::from_bits_truncate(data[4]);
                    let frame_size = u16::from_be_bytes([data[5], data[6]]);
                    CtlEvent::CapsResp(Capabilities {
                        version,
                        codecs,
                        interaction_model: InteractionModel::OnRequest,
                        audio_frame_size: AudioFrameSize(frame_size),
                    })
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::MicOpenError => {
                if data.len() >= MIC_OPEN_ERROR_MIN_LEN {
                    let code = u16::from_be_bytes([data[1], data[2]]);
                    CtlEvent::MicOpenError(MicOpenErrorCode::from(code))
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
        }
    }

    fn on_caps_resp(&mut self, caps: &Capabilities) -> Result<Codec> {
        let codec = Self::negotiate_codec(caps.codecs)?;
        self.selected_codec = Some(codec);
        self.audio_frame_size = caps.audio_frame_size;
        Ok(codec)
    }

    fn decode_audio(&mut self, data: &[u8]) -> Option<AudioFrame> {
        let expected = self.audio_frame_size.0 as usize;
        if data.len() != expected {
            return None;
        }
        if data.len() < FRAME_HEADER_SIZE {
            return None;
        }

        // Parse header: seq(2) + id(1) + predictor(2) + step_index(1)
        let seq = u16::from_be_bytes([data[0], data[1]]);
        let predictor = i16::from_be_bytes([data[3], data[4]]);
        let step_index = data[5];

        // Reset decoder from frame header (v0.4: per-frame reset)
        self.decoder.reset(predictor, step_index);

        // Decode ADPCM bytes
        let decoded = self.decoder.decode_bytes(&data[6..]);

        // Build sample array: predictor + decoded
        let mut samples = Vec::with_capacity(1 + decoded.len());
        samples.push(predictor);
        samples.extend_from_slice(&decoded);

        Some(AudioFrame {
            seq,
            codec: self.selected_codec.unwrap_or(Codec::Adpcm8kHz),
            samples,
        })
    }

    fn on_audio_sync(&mut self, sync: &AudioSyncData) {
        // v0.4: just note the frame number for gap detection
        // (no decoder state in v0.4 AUDIO_SYNC)
        match sync {
            AudioSyncData::FrameNum { .. } => {}
            AudioSyncData::Full { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Command encoding tests ──────────────────────────────────────

    #[test]
    fn test_get_caps_cmd() {
        let p = ProtocolV04::new();
        assert_eq!(p.get_caps_cmd(), vec![0x0A, 0x00, 0x04, 0x00, 0x03]);
    }

    #[test]
    fn test_mic_open_cmd_8khz() {
        let mut p = ProtocolV04::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        assert_eq!(p.mic_open_cmd(), vec![0x0C, 0x00, 0x01]);
    }

    #[test]
    fn test_mic_open_cmd_16khz() {
        let mut p = ProtocolV04::new();
        p.selected_codec = Some(Codec::Adpcm16kHz);
        assert_eq!(p.mic_open_cmd(), vec![0x0C, 0x00, 0x02]);
    }

    #[test]
    fn test_mic_close_cmd_ignores_stream_id() {
        let p = ProtocolV04::new();
        assert_eq!(p.mic_close_cmd(StreamId::MIC_OPEN), vec![0x0D]);
        assert_eq!(p.mic_close_cmd(StreamId::ANY), vec![0x0D]);
    }

    #[test]
    fn test_keepalive_is_mic_open() {
        let mut p = ProtocolV04::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        assert_eq!(p.keepalive_cmd(StreamId::MIC_OPEN), p.mic_open_cmd());
    }

    // ── CTL parsing tests ───────────────────────────────────────────

    #[test]
    fn test_parse_audio_start() {
        let mut p = ProtocolV04::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        match p.parse_ctl(&[0x04]) {
            CtlEvent::AudioStart {
                reason,
                codec,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::MicOpen);
                assert_eq!(codec, Codec::Adpcm8kHz);
                assert_eq!(stream_id, StreamId::MIC_OPEN);
            }
            other => panic!("expected AudioStart, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_stop() {
        let p = ProtocolV04::new();
        match p.parse_ctl(&[0x00]) {
            CtlEvent::AudioStop { reason } => {
                assert!(matches!(reason, AudioStopReason::MicClose));
            }
            other => panic!("expected AudioStop, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_start_search() {
        let p = ProtocolV04::new();
        assert!(matches!(p.parse_ctl(&[0x08]), CtlEvent::StartSearch));
    }

    #[test]
    fn test_parse_audio_sync() {
        let p = ProtocolV04::new();
        // v0.4 AUDIO_SYNC: opcode + frame_num(2)
        match p.parse_ctl(&[0x0A, 0x00, 0x05]) {
            CtlEvent::AudioSync(AudioSyncData::FrameNum { seq }) => {
                assert_eq!(seq, 5);
            }
            other => panic!("expected AudioSync::FrameNum, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_caps_resp() {
        let p = ProtocolV04::new();
        // v0.4 CAPS_RESP: [0x0B, version(2), codecs_supported(2), bytes/frame(2), bytes/char(2)]
        let data = [0x0B, 0x00, 0x04, 0x00, 0x01, 0x00, 0x86, 0x00, 0x14];
        match p.parse_ctl(&data) {
            CtlEvent::CapsResp(caps) => {
                assert_eq!(caps.version, ProtocolVersion::V0_4);
                assert!(caps.codecs.contains(Codecs::ADPCM_8KHZ));
                assert!(!caps.codecs.contains(Codecs::ADPCM_16KHZ));
                assert_eq!(caps.interaction_model, InteractionModel::OnRequest);
                assert_eq!(caps.audio_frame_size.0, 0x0086);
            }
            other => panic!("expected CapsResp, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_mic_open_error() {
        let p = ProtocolV04::new();
        match p.parse_ctl(&[0x0C, 0x0F, 0x01]) {
            CtlEvent::MicOpenError(code) => {
                assert!(matches!(code, MicOpenErrorCode::InvalidCodec));
            }
            other => panic!("expected MicOpenError, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown() {
        let p = ProtocolV04::new();
        assert!(matches!(p.parse_ctl(&[0xFF, 0x01]), CtlEvent::Unknown(_)));
    }

    // ── Codec negotiation tests ─────────────────────────────────────

    #[test]
    fn test_on_caps_resp_selects_best_codec() {
        let mut p = ProtocolV04::new();
        // Remote supports both codecs
        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let codec = p.on_caps_resp(&caps).unwrap();
        assert_eq!(codec, Codec::Adpcm16kHz); // prefers best
    }

    #[test]
    fn test_on_caps_resp_8khz_only() {
        let mut p = ProtocolV04::new();
        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let codec = p.on_caps_resp(&caps).unwrap();
        assert_eq!(codec, Codec::Adpcm8kHz);
    }

    // ── Audio frame decoding tests ──────────────────────────────────

    #[test]
    fn test_decode_audio_frame() {
        let mut p = ProtocolV04::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(134);
        // Build a minimal 134-byte frame with known header
        let mut frame = vec![0u8; 134];
        frame[0] = 0x00;
        frame[1] = 0x07; // seq = 7
                         // frame[2] = 0x00; // id/padding
                         // predictor = 0, step_index = 0
        let result = p.decode_audio(&frame);
        assert!(result.is_some());
        let af = result.unwrap();
        assert_eq!(af.seq, 7);
        assert_eq!(af.samples.len(), 257); // 1 predictor + 256 decoded
    }

    #[test]
    fn test_decode_audio_wrong_size() {
        let mut p = ProtocolV04::new();
        p.audio_frame_size = AudioFrameSize(134);
        assert!(p.decode_audio(&[0u8; 100]).is_none());
    }
}
