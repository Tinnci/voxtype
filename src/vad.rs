//! Dependency-free adaptive energy VAD for 16 kHz mono signed 16-bit PCM.

use std::fs;
use std::io;
use std::path::Path;

pub const BYTES_PER_FRAME: usize = 640;
const PRE_ROLL_FRAMES: u32 = 8;
const POST_ROLL_FRAMES: u32 = 15;
const RELEASE_FRAMES: u32 = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VadConfig {
    /// Absolute lower bound. Adaptive noise tracking may raise this threshold.
    pub rms_threshold: u16,
    /// Consecutive frames required to enter the speech state.
    pub minimum_voiced_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VadResult {
    pub speech_detected: bool,
    pub voiced_frames: u32,
    pub total_frames: u32,
    pub peak: u16,
    pub average_rms: u16,
    pub noise_floor: u16,
    pub adaptive_threshold: u16,
    pub speech_start_frame: Option<u32>,
    pub speech_end_frame: Option<u32>,
    pub trim_start_frame: Option<u32>,
    pub trim_end_frame: Option<u32>,
}

/// Analyzes 16 kHz mono signed 16-bit little-endian PCM in 20 ms frames.
///
/// # Errors
///
/// Returns an I/O error when the recording cannot be read.
pub fn analyze_file(path: &Path, config: VadConfig) -> io::Result<VadResult> {
    let pcm = fs::read(path)?;
    Ok(analyze_pcm(&pcm, config))
}

/// Rewrites a PCM file to the padded speech interval selected by [`analyze_pcm`].
///
/// # Errors
///
/// Returns an I/O error when the recording cannot be read or rewritten.
pub fn trim_file(path: &Path, result: &VadResult) -> io::Result<u64> {
    let (Some(start), Some(end)) = (result.trim_start_frame, result.trim_end_frame) else {
        return Ok(fs::metadata(path)?.len());
    };
    let pcm = fs::read(path)?;
    let start = usize::try_from(start)
        .unwrap_or(usize::MAX)
        .saturating_mul(BYTES_PER_FRAME)
        .min(pcm.len());
    let end = usize::try_from(end)
        .unwrap_or(usize::MAX)
        .saturating_mul(BYTES_PER_FRAME)
        .min(pcm.len());
    if start >= end {
        return Ok(u64::try_from(pcm.len()).unwrap_or(u64::MAX));
    }
    fs::write(path, &pcm[start..end])?;
    Ok(u64::try_from(end - start).unwrap_or(u64::MAX))
}

#[must_use]
pub fn analyze_pcm(pcm: &[u8], config: VadConfig) -> VadResult {
    let (levels, peak) = frame_levels(pcm);
    let total_frames = u32::try_from(levels.len()).unwrap_or(u32::MAX);
    let average_rms = average(&levels);
    if levels.is_empty() {
        return empty_result(peak);
    }

    // A low percentile remains stable when the user starts speaking immediately and
    // avoids treating a single exceptionally quiet frame as the room noise level.
    let mut sorted = levels.clone();
    sorted.sort_unstable();
    let noise_index = sorted.len().saturating_sub(1).saturating_mul(20) / 100;
    let noise_floor = sorted[noise_index];
    let noise_threshold = noise_floor.saturating_mul(2).saturating_add(80);
    // Do not let a speech-dominated short recording classify the voice itself as
    // background noise. Four times the configured floor still rejects common
    // steady fan/room noise while preserving immediate speech onsets.
    let adaptive_threshold = config
        .rms_threshold
        .max(noise_threshold.min(config.rms_threshold.saturating_mul(4)));

    let mut voiced_frames = 0_u32;
    let mut attack = 0_u32;
    let mut release = 0_u32;
    let mut in_speech = false;
    let mut speech_start = None;
    let mut speech_end = None;
    for (index, &rms) in levels.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        if rms >= adaptive_threshold {
            voiced_frames = voiced_frames.saturating_add(1);
            attack = attack.saturating_add(1);
            release = 0;
            if !in_speech && attack >= config.minimum_voiced_frames {
                in_speech = true;
                if speech_start.is_none() {
                    speech_start = Some(index.saturating_add(1).saturating_sub(attack));
                }
            }
            if in_speech {
                speech_end = Some(index.saturating_add(1));
            }
        } else {
            attack = 0;
            if in_speech {
                release = release.saturating_add(1);
                if release >= RELEASE_FRAMES {
                    in_speech = false;
                    release = 0;
                }
            }
        }
    }
    let speech_detected = speech_start.is_some() && speech_end.is_some();
    let trim_start_frame = speech_start.map(|value| value.saturating_sub(PRE_ROLL_FRAMES));
    let trim_end_frame =
        speech_end.map(|value| value.saturating_add(POST_ROLL_FRAMES).min(total_frames));
    VadResult {
        speech_detected,
        voiced_frames,
        total_frames,
        peak,
        average_rms,
        noise_floor,
        adaptive_threshold,
        speech_start_frame: speech_start,
        speech_end_frame: speech_end,
        trim_start_frame,
        trim_end_frame,
    }
}

fn frame_levels(pcm: &[u8]) -> (Vec<u16>, u16) {
    let mut levels = Vec::with_capacity(pcm.len().div_ceil(BYTES_PER_FRAME));
    let mut peak = 0_u16;
    for frame in pcm.chunks(BYTES_PER_FRAME) {
        let mut square_sum = 0_u64;
        let mut samples = 0_u64;
        for bytes in frame.chunks_exact(2) {
            let magnitude = i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs();
            peak = peak.max(magnitude);
            let value = u64::from(magnitude);
            square_sum = square_sum.saturating_add(value.saturating_mul(value));
            samples += 1;
        }
        if let Some(mean_square) = square_sum.checked_div(samples) {
            levels.push(u16::try_from(integer_sqrt(mean_square)).unwrap_or(u16::MAX));
        }
    }
    (levels, peak)
}

fn average(levels: &[u16]) -> u16 {
    if levels.is_empty() {
        return 0;
    }
    let total = levels
        .iter()
        .fold(0_u64, |sum, value| sum.saturating_add(u64::from(*value)));
    u16::try_from(total / u64::try_from(levels.len()).unwrap_or(u64::MAX)).unwrap_or(u16::MAX)
}

const fn empty_result(peak: u16) -> VadResult {
    VadResult {
        speech_detected: false,
        voiced_frames: 0,
        total_frames: 0,
        peak,
        average_rms: 0,
        noise_floor: 0,
        adaptive_threshold: 0,
        speech_start_frame: None,
        speech_end_frame: None,
        trim_start_frame: None,
        trim_end_frame: None,
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

    fn frames(levels: &[i16]) -> Vec<u8> {
        let mut pcm = Vec::new();
        for level in levels {
            for _ in 0..(BYTES_PER_FRAME / 2) {
                pcm.extend_from_slice(&level.to_le_bytes());
            }
        }
        pcm
    }

    #[test]
    fn rejects_silence_and_tracks_noise() {
        let result = analyze_pcm(&frames(&[100; 20]), CONFIG);
        assert!(!result.speech_detected);
        assert_eq!(result.noise_floor, 100);
        assert_eq!(result.adaptive_threshold, 300);
    }

    #[test]
    fn detects_speech_with_padded_trim_boundaries() {
        let mut levels = vec![100; 20];
        levels.extend([2_000; 10]);
        levels.extend([100; 30]);
        let result = analyze_pcm(&frames(&levels), CONFIG);
        assert!(result.speech_detected);
        assert_eq!(result.speech_start_frame, Some(20));
        assert_eq!(result.speech_end_frame, Some(30));
        assert_eq!(result.trim_start_frame, Some(12));
        assert_eq!(result.trim_end_frame, Some(45));
    }

    #[test]
    fn ignores_short_click_without_losing_a_fast_speech_onset() {
        let result = analyze_pcm(&frames(&[2_000, 100, 2_000, 2_000, 2_000, 100]), CONFIG);
        assert!(result.speech_detected);
        assert_eq!(result.speech_start_frame, Some(2));
        assert_eq!(result.trim_start_frame, Some(0));
    }

    #[test]
    fn adaptive_threshold_rejects_steady_loud_background() {
        let mut levels = vec![800; 30];
        levels.extend([1_000; 3]);
        let result = analyze_pcm(&frames(&levels), CONFIG);
        assert!(!result.speech_detected);
        assert_eq!(result.adaptive_threshold, 1_200);
    }

    #[test]
    fn detects_short_recording_dominated_by_speech() {
        let result = analyze_pcm(&frames(&[2_000; 8]), CONFIG);
        assert!(result.speech_detected);
        assert_eq!(result.adaptive_threshold, 1_200);
    }

    #[test]
    fn preserves_first_segment_across_a_long_pause() {
        let mut levels = vec![100; 10];
        levels.extend([2_000; 6]);
        levels.extend([100; 20]);
        levels.extend([2_000; 6]);
        levels.extend([100; 10]);
        let result = analyze_pcm(&frames(&levels), CONFIG);
        assert!(result.speech_detected);
        assert_eq!(result.speech_start_frame, Some(10));
        assert_eq!(result.speech_end_frame, Some(42));
        assert_eq!(result.trim_start_frame, Some(2));
        assert_eq!(result.trim_end_frame, Some(52));
    }

    #[test]
    fn integer_square_root_is_bounded() {
        assert_eq!(integer_sqrt(0), 0);
        assert_eq!(integer_sqrt(2_000_u64.pow(2)), 2_000);
        assert_eq!(integer_sqrt(15), 3);
    }
}
