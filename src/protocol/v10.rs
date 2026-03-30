use anyhow::Result;

use super::types::*;
use super::Protocol;
use crate::adpcm::AdpcmDecoder;

/// ATVV v1.0 protocol implementation.
///
/// Key differences from v0.4:
/// - GET_CAPS includes interaction model support
/// - MIC_OPEN takes audio_mode, not codec
/// - MIC_CLOSE and MIC_EXTEND include stream_id
/// - AUDIO_START/STOP/SYNC have payloads (reason, codec, stream_id, decoder state)
/// - Audio frames are headerless (decoder state persists, reset via AUDIO_SYNC)
///
/// Interaction model support bitmask sent in GET_CAPS:
/// bit 0 = PTT (0x01), bit 1 = HTT (0x02). OnRequest is always implied.
const SUPPORTED_INTERACTION_MODELS: u8 = 0x03;

// v1.0 CTL payload minimum lengths (including opcode byte).
const AUDIO_START_MIN_LEN: usize = 4; // opcode + reason(1) + codec(1) + stream_id(1)
const AUDIO_SYNC_MIN_LEN: usize = 7; // opcode + codec(1) + frame_num(2) + predictor(2) + step_index(1)
const CAPS_RESP_MIN_LEN: usize = 7; // opcode + version(2) + codecs(1) + model(1) + frame_size(2)
const MIC_OPEN_ERROR_MIN_LEN: usize = 3; // opcode + error_code(2)

pub struct ProtocolV10 {
    pub selected_codec: Option<Codec>,
    pub interaction_model: InteractionModel,
    pub audio_frame_size: AudioFrameSize,
    pub decoder: AdpcmDecoder,
    pub frame_seq: u16,
}

impl Default for ProtocolV10 {
    fn default() -> Self {
        Self {
            selected_codec: None,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize::DEFAULT_V10,
            decoder: AdpcmDecoder::new(),
            frame_seq: 0,
        }
    }
}

impl ProtocolV10 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Negotiate the best codec from remote's supported set.
    fn negotiate_codec(remote_codecs: Codecs) -> Result<Codec> {
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

impl Protocol for ProtocolV10 {
    fn version(&self) -> ProtocolVersion {
        ProtocolVersion::V1_0
    }

    fn get_caps_cmd(&self) -> Vec<u8> {
        let ver = ProtocolVersion::V1_0.wire_value().to_be_bytes();
        let codecs = (Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ).bits();
        // interaction_models byte: bit 0 = PTT, bit 1 = HTT (OnRequest is always implied)
        vec![
            u8::from(TxOpcode::GetCaps),
            ver[0],
            ver[1],
            0x00,
            codecs,
            SUPPORTED_INTERACTION_MODELS,
        ]
    }

    fn mic_open_cmd(&self) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicOpen), u8::from(AudioMode::Playback)]
    }

    fn mic_close_cmd(&self, stream_id: StreamId) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicClose), stream_id.0]
    }

    fn keepalive_cmd(&self, stream_id: StreamId) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicExtend), stream_id.0]
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
                // v1.0: [opcode, reason(1)]
                let reason = if data.len() >= 2 {
                    AudioStopReason::from(data[1])
                } else {
                    AudioStopReason::MicClose
                };
                CtlEvent::AudioStop { reason }
            }
            CtlOpcode::AudioStart => {
                // v1.0: [opcode, reason(1), codec(1), stream_id(1)]
                if data.len() >= AUDIO_START_MIN_LEN {
                    let reason =
                        AudioStartReason::try_from(data[1]).unwrap_or(AudioStartReason::MicOpen);
                    let codec = Codec::try_from(data[2]).unwrap_or(Codec::Adpcm8kHz);
                    let stream_id = StreamId(data[3]);
                    CtlEvent::AudioStart {
                        reason,
                        codec,
                        stream_id,
                    }
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::StartSearch => CtlEvent::StartSearch,
            CtlOpcode::AudioSync => {
                // v1.0: [opcode, codec(1), frame_num(2), predictor(2), step_index(1)]
                if data.len() >= AUDIO_SYNC_MIN_LEN {
                    let codec = Codec::try_from(data[1]).unwrap_or(Codec::Adpcm8kHz);
                    let seq = u16::from_be_bytes([data[2], data[3]]);
                    let predictor = i16::from_be_bytes([data[4], data[5]]);
                    let step_index = data[6];
                    CtlEvent::AudioSync(AudioSyncData::Full {
                        codec,
                        seq,
                        predictor,
                        step_index,
                    })
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::CapsResp => {
                // v1.0: [opcode, version(2), codecs(1), interaction_model(1), frame_size(2), extra_config(1), reserved(1)]
                if data.len() >= CAPS_RESP_MIN_LEN {
                    let version_wire = u16::from_be_bytes([data[1], data[2]]);
                    let version =
                        ProtocolVersion::from_wire(version_wire).unwrap_or(ProtocolVersion::V1_0);
                    let codecs = Codecs::from_bits_truncate(data[3]);
                    let interaction_model =
                        InteractionModel::try_from(data[4]).unwrap_or(InteractionModel::OnRequest);
                    let frame_size = u16::from_be_bytes([data[5], data[6]]);
                    CtlEvent::CapsResp(Capabilities {
                        version,
                        codecs,
                        interaction_model,
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
        self.interaction_model = caps.interaction_model;
        self.audio_frame_size = caps.audio_frame_size;
        Ok(codec)
    }

    fn decode_audio(&mut self, data: &[u8]) -> Option<AudioFrame> {
        let expected = self.audio_frame_size.0 as usize;
        if data.len() != expected {
            return None;
        }

        // v1.0: headerless frames. Decoder state persists across frames,
        // reset only via AUDIO_SYNC.
        let samples = self.decoder.decode_bytes(data);
        let seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        Some(AudioFrame {
            seq,
            codec: self.selected_codec.unwrap_or(Codec::Adpcm8kHz),
            samples,
        })
    }

    fn on_audio_sync(&mut self, sync: &AudioSyncData) {
        match sync {
            AudioSyncData::Full {
                codec,
                seq,
                predictor,
                step_index,
            } => {
                self.decoder.reset(*predictor, *step_index);
                self.frame_seq = *seq;
                self.selected_codec = Some(*codec);
            }
            AudioSyncData::FrameNum { seq } => {
                self.frame_seq = *seq;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Command encoding tests ──────────────────────────────────────

    #[test]
    fn test_get_caps_cmd() {
        let p = ProtocolV10::new();
        let cmd = p.get_caps_cmd();
        assert_eq!(cmd[0], 0x0A);
        assert_eq!(cmd[1], 0x01); // version 1.0 high
        assert_eq!(cmd[2], 0x00); // version 1.0 low
        assert_eq!(cmd[3], 0x00); // legacy 0x0003 high
        assert_eq!(cmd[4], 0x03); // legacy 0x0003 low
        assert_eq!(cmd[5], 0x03); // support HTT+PTT+OnRequest
    }

    #[test]
    fn test_mic_open_cmd() {
        let p = ProtocolV10::new();
        // v1.0 MIC_OPEN: [0x0C, audio_mode]
        assert_eq!(p.mic_open_cmd(), vec![0x0C, 0x00]); // Playback mode
    }

    #[test]
    fn test_mic_close_cmd_with_stream_id() {
        let p = ProtocolV10::new();
        assert_eq!(p.mic_close_cmd(StreamId::MIC_OPEN), vec![0x0D, 0x00]);
        assert_eq!(p.mic_close_cmd(StreamId(0x05)), vec![0x0D, 0x05]);
        assert_eq!(p.mic_close_cmd(StreamId::ANY), vec![0x0D, 0xFF]);
    }

    #[test]
    fn test_keepalive_is_mic_extend() {
        let p = ProtocolV10::new();
        assert_eq!(p.keepalive_cmd(StreamId::MIC_OPEN), vec![0x0E, 0x00]);
        assert_eq!(p.keepalive_cmd(StreamId(0x03)), vec![0x0E, 0x03]);
    }

    // ── CTL parsing tests ───────────────────────────────────────────

    #[test]
    fn test_parse_audio_start_mic_open() {
        let p = ProtocolV10::new();
        // AUDIO_START: [0x04, reason=0x00, codec=0x01, stream_id=0x00]
        match p.parse_ctl(&[0x04, 0x00, 0x01, 0x00]) {
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
    fn test_parse_audio_start_ptt() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x04, 0x01, 0x02, 0x01]) {
            CtlEvent::AudioStart {
                reason,
                codec,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::PressToTalk);
                assert_eq!(codec, Codec::Adpcm16kHz);
                assert_eq!(stream_id, StreamId(0x01));
            }
            other => panic!("expected AudioStart PTT, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_start_htt() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x04, 0x03, 0x01, 0x02]) {
            CtlEvent::AudioStart {
                reason,
                codec,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::HoldToTalk);
                assert_eq!(stream_id, StreamId(0x02));
            }
            other => panic!("expected AudioStart HTT, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_stop_with_reason() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x00, 0x02]) {
            CtlEvent::AudioStop { reason } => {
                assert!(matches!(reason, AudioStopReason::HttButtonRelease));
            }
            other => panic!("expected AudioStop, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_sync_full() {
        let p = ProtocolV10::new();
        // [0x0A, codec=0x01, frame_hi, frame_lo, pred_hi, pred_lo, step_index]
        match p.parse_ctl(&[0x0A, 0x01, 0x00, 0x0A, 0x03, 0xE8, 0x20]) {
            CtlEvent::AudioSync(AudioSyncData::Full {
                codec,
                seq,
                predictor,
                step_index,
            }) => {
                assert_eq!(codec, Codec::Adpcm8kHz);
                assert_eq!(seq, 10);
                assert_eq!(predictor, 1000); // 0x03E8
                assert_eq!(step_index, 32);
            }
            other => panic!("expected AudioSync::Full, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_caps_resp_v10() {
        let p = ProtocolV10::new();
        // [0x0B, version(2), codecs(1), interaction_model(1), frame_size(2), extra_config(1), reserved(1)]
        let data = [0x0B, 0x01, 0x00, 0x03, 0x03, 0x00, 0xA0, 0x01, 0x00];
        match p.parse_ctl(&data) {
            CtlEvent::CapsResp(caps) => {
                assert_eq!(caps.version, ProtocolVersion::V1_0);
                assert_eq!(caps.codecs, Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ);
                assert_eq!(caps.interaction_model, InteractionModel::HoldToTalk);
                assert_eq!(caps.audio_frame_size.0, 160);
            }
            other => panic!("expected CapsResp, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_mic_open_error_ptt_in_progress() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x0C, 0x0F, 0x80]) {
            CtlEvent::MicOpenError(code) => {
                assert!(matches!(code, MicOpenErrorCode::PttHttInProgress));
            }
            other => panic!("expected MicOpenError, got {:?}", other),
        }
    }

    // ── Audio sync and headerless frame decoding tests ──────────────

    #[test]
    fn test_on_audio_sync_resets_decoder() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        let sync = AudioSyncData::Full {
            codec: Codec::Adpcm8kHz,
            seq: 42,
            predictor: 500,
            step_index: 30,
        };
        p.on_audio_sync(&sync);
        assert_eq!(p.decoder.predictor, 500);
        assert_eq!(p.decoder.step_index, 30);
        assert_eq!(p.frame_seq, 42);
    }

    #[test]
    fn test_decode_headerless_frame() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        p.decoder.reset(0, 0);
        p.frame_seq = 0;

        let data = vec![0u8; 20]; // 20 bytes = 40 samples
        let result = p.decode_audio(&data);
        assert!(result.is_some());
        let af = result.unwrap();
        assert_eq!(af.seq, 0);
        assert_eq!(af.samples.len(), 40);
        // frame_seq should have incremented
        assert_eq!(p.frame_seq, 1);
    }
}
