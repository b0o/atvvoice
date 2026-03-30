// Wire protocol types for ATVV v0.4 and v1.0.
// All types use strong typing: repr(u8) enums with num_enum derives,
// bitflags for codec bitmasks, newtypes for stream IDs and frame sizes.

use num_enum::{IntoPrimitive, TryFromPrimitive};

/// ATVV protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    V0_4,
    V1_0,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V0_4 => write!(f, "v0.4"),
            Self::V1_0 => write!(f, "v1.0"),
        }
    }
}

impl ProtocolVersion {
    /// Parse a version string like "0.4" or "1.0".
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "0.4" => Ok(Self::V0_4),
            "1.0" => Ok(Self::V1_0),
            _ => Err(format!(
                "unknown protocol version {s:?}, expected \"0.4\" or \"1.0\""
            )),
        }
    }

    /// Wire format: major.minor as big-endian u16.
    pub fn wire_value(self) -> u16 {
        match self {
            Self::V0_4 => 0x0004,
            Self::V1_0 => 0x0100,
        }
    }

    /// Parse from wire format (big-endian u16). Returns None for unknown versions.
    pub fn from_wire(value: u16) -> Option<Self> {
        match value {
            0x0004 => Some(Self::V0_4),
            0x0100 => Some(Self::V1_0),
            _ => None,
        }
    }
}

/// TX opcodes (host -> remote, written to TX characteristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive)]
#[repr(u8)]
pub enum TxOpcode {
    GetCaps = 0x0A,
    MicOpen = 0x0C,
    MicClose = 0x0D,
    MicExtend = 0x0E,
}

/// CTL opcodes (remote -> host, received on CTL characteristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum CtlOpcode {
    AudioStop = 0x00,
    AudioStart = 0x04,
    StartSearch = 0x08,
    AudioSync = 0x0A,
    CapsResp = 0x0B,
    MicOpenError = 0x0C,
}

/// Individual codec identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Codec {
    Adpcm8kHz = 0x01,
    Adpcm16kHz = 0x02,
}

impl Codec {
    /// Sample rate in Hz for this codec.
    pub fn sample_rate(self) -> u32 {
        match self {
            Self::Adpcm8kHz => 8000,
            Self::Adpcm16kHz => 16000,
        }
    }
}

bitflags::bitflags! {
    /// Codec support bitmask (from CAPS_RESP).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Codecs: u8 {
        const ADPCM_8KHZ  = 0x01;
        const ADPCM_16KHZ = 0x02;
    }
}

/// Audio frame size in bytes (from CAPS_RESP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFrameSize(pub u16);

impl AudioFrameSize {
    pub const DEFAULT_V04: Self = Self(134); // 6-byte header + 128 ADPCM
    pub const DEFAULT_V10: Self = Self(20); // default notification payload
}

/// Stream identifier. Race-condition guard, not multiplexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamId(pub u8);

impl StreamId {
    /// Stream initiated by MIC_OPEN.
    pub const MIC_OPEN: Self = Self(0x00);
    /// Wildcard: close any active stream.
    pub const ANY: Self = Self(0xFF);
}

/// Interaction model (v1.0 only; v0.4 is always OnRequest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum InteractionModel {
    OnRequest = 0x00,
    PressToTalk = 0x01,
    HoldToTalk = 0x03,
}

/// Audio buffering mode for v1.0 MIC_OPEN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum AudioMode {
    Playback = 0x00, // realtime, reduced buffer
    Capture = 0x01,  // non-realtime, larger buffer
}

/// Reason for AUDIO_START (v1.0 payload; v0.4 synthesizes MicOpen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum AudioStartReason {
    MicOpen = 0x00,
    PressToTalk = 0x01,
    HoldToTalk = 0x03,
}

/// Reason for AUDIO_STOP. Hand-written From<u8> for catch-all variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStopReason {
    MicClose,              // 0x00
    HttButtonRelease,      // 0x02
    UpcomingAudioStart,    // 0x04
    TransferTimeout,       // 0x08
    NotificationsDisabled, // 0x10
    Other(u8),
}

impl From<u8> for AudioStopReason {
    fn from(v: u8) -> Self {
        match v {
            0x00 => Self::MicClose,
            0x02 => Self::HttButtonRelease,
            0x04 => Self::UpcomingAudioStart,
            0x08 => Self::TransferTimeout,
            0x10 => Self::NotificationsDisabled,
            other => Self::Other(other),
        }
    }
}

/// MIC_OPEN error code. Hand-written From<u16> for catch-all variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicOpenErrorCode {
    InvalidCodec,          // 0x0F01
    RemoteNotActive,       // 0x0F02
    NotificationsDisabled, // 0x0F03
    PttHttInProgress,      // 0x0F80
    InternalError,         // 0x0FFF
    Unknown(u16),
}

impl From<u16> for MicOpenErrorCode {
    fn from(v: u16) -> Self {
        match v {
            0x0F01 => Self::InvalidCodec,
            0x0F02 => Self::RemoteNotActive,
            0x0F03 => Self::NotificationsDisabled,
            0x0F80 => Self::PttHttInProgress,
            0x0FFF => Self::InternalError,
            other => Self::Unknown(other),
        }
    }
}

/// Parsed capabilities from CAPS_RESP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub version: ProtocolVersion,
    pub codecs: Codecs,
    pub interaction_model: InteractionModel,
    pub audio_frame_size: AudioFrameSize,
}

/// Parsed AUDIO_SYNC data. Version-dependent content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSyncData {
    /// v0.4: only frame number.
    FrameNum { seq: u16 },
    /// v1.0: full decoder state for resync.
    Full {
        codec: Codec,
        seq: u16,
        predictor: i16,
        step_index: u8,
    },
}

/// Typed CTL event. The Protocol trait's parse_ctl returns this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlEvent {
    AudioStart {
        reason: AudioStartReason,
        codec: Codec,
        stream_id: StreamId,
    },
    AudioStop {
        reason: AudioStopReason,
    },
    StartSearch,
    AudioSync(AudioSyncData),
    CapsResp(Capabilities),
    MicOpenError(MicOpenErrorCode),
    Unknown(Vec<u8>),
}

/// Decoded audio frame output from Protocol::decode_audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub seq: u16,
    pub codec: Codec,
    pub samples: Vec<i16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_opcode_to_u8() {
        assert_eq!(u8::from(TxOpcode::GetCaps), 0x0A);
        assert_eq!(u8::from(TxOpcode::MicOpen), 0x0C);
        assert_eq!(u8::from(TxOpcode::MicClose), 0x0D);
        assert_eq!(u8::from(TxOpcode::MicExtend), 0x0E);
    }

    #[test]
    fn test_ctl_opcode_from_u8() {
        assert_eq!(CtlOpcode::try_from(0x00), Ok(CtlOpcode::AudioStop));
        assert_eq!(CtlOpcode::try_from(0x04), Ok(CtlOpcode::AudioStart));
        assert_eq!(CtlOpcode::try_from(0x08), Ok(CtlOpcode::StartSearch));
        assert_eq!(CtlOpcode::try_from(0x0A), Ok(CtlOpcode::AudioSync));
        assert_eq!(CtlOpcode::try_from(0x0B), Ok(CtlOpcode::CapsResp));
        assert_eq!(CtlOpcode::try_from(0x0C), Ok(CtlOpcode::MicOpenError));
        assert!(CtlOpcode::try_from(0xFF).is_err());
    }

    #[test]
    fn test_codec_values() {
        assert_eq!(u8::from(Codec::Adpcm8kHz), 0x01);
        assert_eq!(u8::from(Codec::Adpcm16kHz), 0x02);
        assert_eq!(Codec::Adpcm8kHz.sample_rate(), 8000);
        assert_eq!(Codec::Adpcm16kHz.sample_rate(), 16000);
    }

    #[test]
    fn test_codecs_bitmask() {
        let both = Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ;
        assert!(both.contains(Codecs::ADPCM_8KHZ));
        assert!(both.contains(Codecs::ADPCM_16KHZ));
        assert_eq!(both.bits(), 0x03);
        let eight_only = Codecs::from_bits_truncate(0x01);
        assert!(eight_only.contains(Codecs::ADPCM_8KHZ));
        assert!(!eight_only.contains(Codecs::ADPCM_16KHZ));
    }

    #[test]
    fn test_stream_id_constants() {
        assert_eq!(StreamId::MIC_OPEN.0, 0x00);
        assert_eq!(StreamId::ANY.0, 0xFF);
    }

    #[test]
    fn test_audio_start_reason() {
        assert_eq!(
            AudioStartReason::try_from(0x00),
            Ok(AudioStartReason::MicOpen)
        );
        assert_eq!(
            AudioStartReason::try_from(0x01),
            Ok(AudioStartReason::PressToTalk)
        );
        assert_eq!(
            AudioStartReason::try_from(0x03),
            Ok(AudioStartReason::HoldToTalk)
        );
        assert!(AudioStartReason::try_from(0x02).is_err());
    }

    #[test]
    fn test_audio_stop_reason() {
        assert!(matches!(
            AudioStopReason::from(0x00),
            AudioStopReason::MicClose
        ));
        assert!(matches!(
            AudioStopReason::from(0x02),
            AudioStopReason::HttButtonRelease
        ));
        assert!(matches!(
            AudioStopReason::from(0x04),
            AudioStopReason::UpcomingAudioStart
        ));
        assert!(matches!(
            AudioStopReason::from(0x08),
            AudioStopReason::TransferTimeout
        ));
        assert!(matches!(
            AudioStopReason::from(0x10),
            AudioStopReason::NotificationsDisabled
        ));
        assert!(matches!(
            AudioStopReason::from(0x80),
            AudioStopReason::Other(0x80)
        ));
        assert!(matches!(
            AudioStopReason::from(0x99),
            AudioStopReason::Other(0x99)
        ));
    }

    #[test]
    fn test_mic_open_error_code() {
        assert!(matches!(
            MicOpenErrorCode::from(0x0F01),
            MicOpenErrorCode::InvalidCodec
        ));
        assert!(matches!(
            MicOpenErrorCode::from(0x0F02),
            MicOpenErrorCode::RemoteNotActive
        ));
        assert!(matches!(
            MicOpenErrorCode::from(0x0F03),
            MicOpenErrorCode::NotificationsDisabled
        ));
        assert!(matches!(
            MicOpenErrorCode::from(0x0F80),
            MicOpenErrorCode::PttHttInProgress
        ));
        assert!(matches!(
            MicOpenErrorCode::from(0x0FFF),
            MicOpenErrorCode::InternalError
        ));
        assert!(matches!(
            MicOpenErrorCode::from(0x1234),
            MicOpenErrorCode::Unknown(0x1234)
        ));
    }

    #[test]
    fn test_protocol_version_parse() {
        assert_eq!(ProtocolVersion::parse("0.4"), Ok(ProtocolVersion::V0_4));
        assert_eq!(ProtocolVersion::parse("1.0"), Ok(ProtocolVersion::V1_0));
        assert!(ProtocolVersion::parse("2.0").is_err());
        assert!(ProtocolVersion::parse("abc").is_err());
    }

    #[test]
    fn test_interaction_model() {
        assert_eq!(
            InteractionModel::try_from(0x00),
            Ok(InteractionModel::OnRequest)
        );
        assert_eq!(
            InteractionModel::try_from(0x01),
            Ok(InteractionModel::PressToTalk)
        );
        assert_eq!(
            InteractionModel::try_from(0x03),
            Ok(InteractionModel::HoldToTalk)
        );
    }

    #[test]
    fn test_audio_mode() {
        assert_eq!(u8::from(AudioMode::Playback), 0x00);
        assert_eq!(u8::from(AudioMode::Capture), 0x01);
    }
}
