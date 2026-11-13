//! Which encoder a build records with.

use examic_audio::{EncodeError, PacketEncoder};

/// 32 kbit/s VBR mono: transparent for speech, about 14 MB per hour.
#[cfg(target_os = "android")]
const BITRATE_BPS: i32 = 32_000;

#[cfg(target_os = "android")]
pub fn new_encoder(sample_rate: u32) -> Result<impl PacketEncoder + 'static, EncodeError> {
    examic_audio::OpusEncoder::new(sample_rate, BITRATE_BPS)
}

/// Off Android this crate is built only to run the engine's tests, and those
/// must not need CMake and a C compiler. The stand-in keeps Opus framing (20 ms
/// frames, 6.5 ms lookahead) but its packets are not decodable audio.
#[cfg(not(target_os = "android"))]
pub fn new_encoder(sample_rate: u32) -> Result<impl PacketEncoder + 'static, EncodeError> {
    Ok(FramingStub { sample_rate })
}

#[cfg(not(target_os = "android"))]
struct FramingStub {
    sample_rate: u32,
}

#[cfg(not(target_os = "android"))]
impl PacketEncoder for FramingStub {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn frame_samples(&self) -> usize {
        self.sample_rate as usize / 50
    }

    fn lookahead_samples(&self) -> u32 {
        self.sample_rate * 13 / 2000
    }

    fn encode(&mut self, pcm: &[i16], packet: &mut Vec<u8>) -> Result<(), EncodeError> {
        packet.clear();
        packet.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        Ok(())
    }
}
