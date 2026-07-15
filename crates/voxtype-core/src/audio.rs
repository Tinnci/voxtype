//! Provider-neutral audio types.

/// Audio sample representation at an adapter boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleFormat {
    I16Le,
    F32Le,
}

/// Format of one stream of audio chunks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl AudioFormat {
    pub const PCM_16KHZ_MONO: Self = Self {
        sample_rate_hz: 16_000,
        channels: 1,
        sample_format: SampleFormat::I16Le,
    };
}

/// Owned audio passed between bounded queues.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioChunk {
    pub sequence: u64,
    pub captured_at_micros: u64,
    pub format: AudioFormat,
    pub samples: Vec<u8>,
}

impl AudioChunk {
    #[must_use]
    pub fn duration_micros(&self) -> Option<u64> {
        let bytes_per_sample = match self.format.sample_format {
            SampleFormat::I16Le => 2_u64,
            SampleFormat::F32Le => 4_u64,
        };
        let bytes_per_frame = bytes_per_sample.checked_mul(u64::from(self.format.channels))?;
        let frames = u64::try_from(self.samples.len()).ok()? / bytes_per_frame;
        frames
            .checked_mul(1_000_000)?
            .checked_div(u64::from(self.format.sample_rate_hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_twenty_millisecond_pcm_frame() {
        let chunk = AudioChunk {
            sequence: 0,
            captured_at_micros: 0,
            format: AudioFormat::PCM_16KHZ_MONO,
            samples: vec![0; 640],
        };

        assert_eq!(chunk.duration_micros(), Some(20_000));
    }
}
