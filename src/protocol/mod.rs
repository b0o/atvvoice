pub mod types;
pub mod v04;
pub mod v10;

use anyhow::Result;
use types::*;

/// Protocol abstraction for ATVV v0.4 and v1.0.
///
/// Implementations encode commands, parse CTL notifications, negotiate codecs,
/// and decode audio frames according to their spec version.
pub trait Protocol: Send {
    /// Protocol version this implementation speaks.
    fn version(&self) -> ProtocolVersion;

    /// Build the GET_CAPS command bytes (including opcode).
    fn get_caps_cmd(&self) -> Vec<u8>;

    /// Build MIC_OPEN command bytes.
    fn mic_open_cmd(&self) -> Vec<u8>;

    /// Build MIC_CLOSE command bytes for a given stream.
    fn mic_close_cmd(&self, stream_id: StreamId) -> Vec<u8>;

    /// Build keepalive command bytes.
    /// v1.0: MIC_EXTEND. v0.4: MIC_OPEN (no MIC_EXTEND support).
    fn keepalive_cmd(&self, stream_id: StreamId) -> Vec<u8>;

    /// Parse a CTL notification into a typed event.
    fn parse_ctl(&self, data: &[u8]) -> CtlEvent;

    /// Process received capabilities. Returns the negotiated codec.
    fn on_caps_resp(&mut self, caps: &Capabilities) -> Result<Codec>;

    /// Decode an audio frame notification into PCM samples.
    /// Returns None if the frame is invalid/wrong size.
    fn decode_audio(&mut self, data: &[u8]) -> Option<AudioFrame>;

    /// Handle AUDIO_SYNC (update internal decoder state).
    fn on_audio_sync(&mut self, sync: &AudioSyncData);
}

/// Create a Protocol implementation for the given version.
pub fn create_protocol(version: ProtocolVersion) -> Box<dyn Protocol> {
    match version {
        ProtocolVersion::V0_4 => Box::new(v04::ProtocolV04::new()),
        ProtocolVersion::V1_0 => Box::new(v10::ProtocolV10::new()),
    }
}
