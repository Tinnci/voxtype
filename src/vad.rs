//! Dependency-free energy voice activity detection for captured PCM.

use std::fs;
use std::io;
use std::path::Path;

const BYTES_PER_FRAME: usize = 640;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VadConfig {
    pub rms_threshold: u16,
    pub minimum_voiced_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VadResult {
    pub speech_detected: bool,
    pub voiced_frames: u32,
    pub total_frames: u32,
    pub peak: u16,
    pub average_rms: u16,
}

/// Analyzes 16 kHz mono signed 16-bit little-endian PCM in 20 ms frames.
///
/// # Errors
///
/// Returns an I/O error if the recording cannot be read.
pub fn analyze_file(path: &Path, config: VadConfig) -> io::Result<VadResult> {
    let pcm = fs::read(path)?;
    Ok(analyze_pcm(&pcm, config))
}

#[must_use]
pub fn analyze_pcm(pcm: &[u8], config: VadConfig) -> VadResult {
    let mut voiced_frames = 0_u32;
    let mut total_frames = 0_u32;
    let mut peak = 0_u16;
    let mut rms_total = 0_u64;

    for frame in pcm.chunks(BYTES_PER_FRAME) {
        if frame.len() < 2 {
            continue;
        }
        let mut square_sum = 0_u64;
        let mut samples = 0_u64;
        for bytes in frame.chunks_exact(2) {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            let magnitude = sample.unsigned_abs();
            peak = peak.max(magnitude);
            let value = u64::from(magnitude);
            square_sum = square_sum.saturating_add(value.saturating_mul(value));
            samples += 1;
        }
        if samples == 0 {
            continue;
        }
        let rms = u16::try_from(integer_sqrt(square_sum / samples)).unwrap_or(u16::MAX);
        rms_total = rms_total.saturating_add(u64::from(rms));
        total_frames = total_frames.saturating_add(1);
        if rms >= config.rms_threshold {
            voiced_frames = voiced_frames.saturating_add(1);
        }
    }

    let average_rms = if total_frames == 0 {
        0
    } else {
        u16::try_from(rms_total / u64::from(total_frames)).unwrap_or(u16::MAX)
    };
    VadResult {
        speech_detected: voiced_frames >= config.minimum_voiced_frames,
        voiced_frames,
        total_frames,
        peak,
        average_rms,
    }
}

fn integer_sqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut estimate = value;
    let mut next = u64::midpoint(estimate, value / estimate);
    while next < estimate {
        estimate = next;
        next = u64::midpoint(estimate, value / estimate);
    }
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: VadConfig = VadConfig {
        rms_threshold: 300,
        minimum_voiced_frames: 2,
    };

    #[test]
    fn rejects_silence() {
        let result = analyze_pcm(&vec![0; BYTES_PER_FRAME * 4], CONFIG);
        assert!(!result.speech_detected);
        assert_eq!(result.peak, 0);
    }

    #[test]
    fn detects_sustained_signal() {
        let mut pcm = Vec::new();
        for _ in 0..640 {
            pcm.extend_from_slice(&2_000_i16.to_le_bytes());
        }
        let result = analyze_pcm(&pcm, CONFIG);
        assert!(result.speech_detected);
        assert_eq!(result.voiced_frames, 2);
        assert_eq!(result.peak, 2_000);
    }

    #[test]
    fn integer_square_root_is_bounded() {
        assert_eq!(integer_sqrt(0), 0);
        assert_eq!(integer_sqrt(2_000_u64.pow(2)), 2_000);
        assert_eq!(integer_sqrt(15), 3);
    }
}
