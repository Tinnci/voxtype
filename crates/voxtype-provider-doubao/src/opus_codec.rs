//! Exact 20 ms mono 16 kHz PCM to raw Opus packet encoding.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use opus::{Application, Channels, Encoder};

use crate::PCM_FRAME_BYTES;

pub const PCM_SAMPLES_PER_FRAME: usize = 320;
pub const MAX_OPUS_PACKET_BYTES: usize = 1275;

/// Replaceable frame encoder used by the session transport.
///
/// Protocol tests can supply a deterministic implementation while production
/// builds use [`SystemOpusEncoder`]. Keeping this boundary packet-oriented also
/// prevents Ogg/container framing from leaking into the WebSocket protocol.
pub trait OpusFrameEncoder {
    /// Encodes exactly one 20 ms little-endian S16 mono PCM frame.
    ///
    /// # Errors
    ///
    /// Returns a stable codec error if the encoder rejects the frame or emits
    /// an empty/oversized packet.
    fn encode_20ms(&mut self, pcm: &[u8; PCM_FRAME_BYTES]) -> Result<Vec<u8>, OpusCodecError>;
}

/// Safe Rust wrapper around the system `libopus` encoder.
#[derive(Debug)]
pub struct SystemOpusEncoder {
    encoder: Encoder,
}

impl SystemOpusEncoder {
    /// Creates a mono 16 kHz encoder tuned for spoken voice.
    ///
    /// # Errors
    ///
    /// Returns a stable error if the installed `libopus` cannot create the
    /// requested encoder.
    pub fn new() -> Result<Self, OpusCodecError> {
        Encoder::new(16_000, Channels::Mono, Application::Voip)
            .map(|encoder| Self { encoder })
            .map_err(|_| OpusCodecError("could not initialize the Opus encoder"))
    }
}

impl OpusFrameEncoder for SystemOpusEncoder {
    fn encode_20ms(&mut self, pcm: &[u8; PCM_FRAME_BYTES]) -> Result<Vec<u8>, OpusCodecError> {
        let mut samples = [0_i16; PCM_SAMPLES_PER_FRAME];
        for (sample, bytes) in samples.iter_mut().zip(pcm.chunks_exact(2)) {
            *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
        }
        let mut packet = vec![0_u8; MAX_OPUS_PACKET_BYTES];
        let encoded = self
            .encoder
            .encode(&samples, &mut packet)
            .map_err(|_| OpusCodecError("could not encode a 20 ms Opus frame"))?;
        if encoded == 0 || encoded > MAX_OPUS_PACKET_BYTES {
            return Err(OpusCodecError(
                "Opus encoder returned an invalid packet size",
            ));
        }
        packet.truncate(encoded);
        Ok(packet)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusCodecError(&'static str);

impl Display for OpusCodecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for OpusCodecError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_encoder_emits_one_decodable_twenty_millisecond_packet() {
        let mut pcm = [0_u8; PCM_FRAME_BYTES];
        for (index, bytes) in pcm.chunks_exact_mut(2).enumerate() {
            let sample = if index % 32 < 16 {
                8_000_i16
            } else {
                -8_000_i16
            };
            bytes.copy_from_slice(&sample.to_le_bytes());
        }

        let packet = SystemOpusEncoder::new()
            .expect("system Opus encoder")
            .encode_20ms(&pcm)
            .expect("encoded packet");
        assert!(!packet.is_empty());
        assert!(packet.len() <= MAX_OPUS_PACKET_BYTES);

        let mut decoder = opus::Decoder::new(16_000, Channels::Mono).expect("Opus decoder");
        let mut output_samples = [0_i16; PCM_SAMPLES_PER_FRAME];
        let samples = decoder
            .decode(&packet, &mut output_samples, false)
            .expect("decode generated packet");
        assert_eq!(samples, PCM_SAMPLES_PER_FRAME);
    }

    #[test]
    fn silence_is_still_transmitted_as_a_valid_packet() {
        let packet = SystemOpusEncoder::new()
            .expect("system Opus encoder")
            .encode_20ms(&[0_u8; PCM_FRAME_BYTES])
            .expect("encoded silence");
        assert!(!packet.is_empty());
    }
}
