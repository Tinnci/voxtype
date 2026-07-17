//! Pure two-phase microphone calibration for 16 kHz mono signed 16-bit PCM.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

const BYTES_PER_FRAME: usize = 640;
const MIN_PHASE_FRAMES: usize = 25;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationConfidence {
    High,
    Medium,
    Low,
}

impl CalibrationConfidence {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationReason {
    Good,
    MarginalSnr,
    NoSpeech,
    TooQuiet,
    Clipping,
    UnstableNoise,
}

impl CalibrationReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::MarginalSnr => "marginal-snr",
            Self::NoSpeech => "no-speech",
            Self::TooQuiet => "too-quiet",
            Self::Clipping => "clipping",
            Self::UnstableNoise => "unstable-noise",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalibrationResult {
    pub noise_p20: u16,
    pub noise_p50: u16,
    pub noise_p95: u16,
    pub speech_p50: u16,
    pub speech_p95: u16,
    pub peak: u16,
    pub snr_db: f64,
    pub clipping_ratio: f64,
    pub speech_ratio: f64,
    pub suggested_threshold: u16,
    pub confidence: CalibrationConfidence,
    pub reason: CalibrationReason,
    pub can_apply: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalibrationError(&'static str);

impl Display for CalibrationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for CalibrationError {}

/// Analyzes an explicit quiet phase followed by an explicit speaking phase.
///
/// # Errors
///
/// Returns an error when either phase contains less than 500 ms of complete
/// 20 ms PCM frames.
pub fn analyze(
    silence_pcm: &[u8],
    speech_pcm: &[u8],
) -> Result<CalibrationResult, CalibrationError> {
    let silence = phase_stats(silence_pcm);
    let speech = phase_stats(speech_pcm);
    if silence.levels.len() < MIN_PHASE_FRAMES || speech.levels.len() < MIN_PHASE_FRAMES {
        return Err(CalibrationError("calibration phases are too short"));
    }

    let noise_p20 = percentile(&silence.levels, 20);
    let noise_p50 = percentile(&silence.levels, 50);
    let noise_p95 = percentile(&silence.levels, 95);
    let speech_p50 = percentile(&speech.levels, 50);
    let speech_p95 = percentile(&speech.levels, 95);
    let peak = silence.peak.max(speech.peak);
    let total_samples = silence.samples.saturating_add(speech.samples);
    let clipped_samples = silence
        .clipped_samples
        .saturating_add(speech.clipped_samples);
    let clipping_ratio = ratio(clipped_samples, total_samples);
    let noise_reference = f64::from(noise_p50.max(1));
    let speech_reference = f64::from(speech_p50.max(1));
    let snr_db = 20.0 * (speech_reference / noise_reference).log10();
    let noise_unstable = noise_p95 > noise_p20.saturating_mul(3).saturating_add(200);
    let no_speech = speech_p50 <= noise_p95.saturating_add(100) || snr_db < 6.0;
    let too_quiet = speech_p95 < 500;
    let clipping = clipping_ratio >= 0.01 || peak >= 32_700;
    let reason = if clipping {
        CalibrationReason::Clipping
    } else if no_speech {
        CalibrationReason::NoSpeech
    } else if too_quiet {
        CalibrationReason::TooQuiet
    } else if noise_unstable {
        CalibrationReason::UnstableNoise
    } else if snr_db < 12.0 {
        CalibrationReason::MarginalSnr
    } else {
        CalibrationReason::Good
    };
    let confidence = match reason {
        CalibrationReason::Good if snr_db >= 16.0 && clipping_ratio < 0.001 => {
            CalibrationConfidence::High
        }
        CalibrationReason::Good | CalibrationReason::MarginalSnr => CalibrationConfidence::Medium,
        CalibrationReason::NoSpeech
        | CalibrationReason::TooQuiet
        | CalibrationReason::Clipping
        | CalibrationReason::UnstableNoise => CalibrationConfidence::Low,
    };
    let suggested_threshold = suggested_threshold(noise_p95, speech_p50);
    let voiced_frames = speech
        .levels
        .iter()
        .filter(|level| **level >= suggested_threshold)
        .count();
    let speech_ratio = ratio(
        u64::try_from(voiced_frames).unwrap_or(u64::MAX),
        u64::try_from(speech.levels.len()).unwrap_or(u64::MAX),
    );
    let can_apply = confidence != CalibrationConfidence::Low
        && speech_ratio >= 0.2
        && suggested_threshold > noise_p95;

    Ok(CalibrationResult {
        noise_p20,
        noise_p50,
        noise_p95,
        speech_p50,
        speech_p95,
        peak,
        snr_db,
        clipping_ratio,
        speech_ratio,
        suggested_threshold,
        confidence,
        reason,
        can_apply,
    })
}

fn suggested_threshold(noise_p95: u16, speech_p50: u16) -> u16 {
    if speech_p50 <= noise_p95 {
        return noise_p95.saturating_add(80).min(10_000);
    }
    let separation = speech_p50.saturating_sub(noise_p95);
    noise_p95
        .saturating_add((separation / 4).max(80))
        .min(10_000)
}

#[derive(Default)]
struct PhaseStats {
    levels: Vec<u16>,
    peak: u16,
    clipped_samples: u64,
    samples: u64,
}

fn phase_stats(pcm: &[u8]) -> PhaseStats {
    let mut stats = PhaseStats::default();
    for frame in pcm.chunks_exact(BYTES_PER_FRAME) {
        let mut square_sum = 0_u64;
        let mut frame_samples = 0_u64;
        for bytes in frame.chunks_exact(2) {
            let magnitude = i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs();
            stats.peak = stats.peak.max(magnitude);
            if magnitude >= 32_700 {
                stats.clipped_samples = stats.clipped_samples.saturating_add(1);
            }
            let value = u64::from(magnitude);
            square_sum = square_sum.saturating_add(value.saturating_mul(value));
            frame_samples = frame_samples.saturating_add(1);
        }
        stats.samples = stats.samples.saturating_add(frame_samples);
        let rms = square_sum
            .checked_div(frame_samples)
            .map(integer_sqrt)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or_default();
        stats.levels.push(rms);
    }
    stats
}

fn percentile(values: &[u16], percentile: usize) -> u16 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = sorted.len().saturating_sub(1).saturating_mul(percentile) / 100;
    sorted[index]
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        let numerator = u32::try_from(numerator).unwrap_or(u32::MAX);
        let denominator = u32::try_from(denominator).unwrap_or(u32::MAX);
        f64::from(numerator) / f64::from(denominator)
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
    fn accepts_stable_noise_and_clear_speech() {
        let result = analyze(&frames(&[100; 75]), &frames(&[2_000; 150])).expect("calibration");
        assert_eq!(result.confidence, CalibrationConfidence::High);
        assert_eq!(result.reason, CalibrationReason::Good);
        assert!(result.snr_db > 20.0);
        assert!(result.suggested_threshold > result.noise_p95);
        assert!(result.suggested_threshold < result.speech_p50);
        assert!(result.can_apply);
    }

    #[test]
    fn rejects_a_speaking_phase_without_speech() {
        let result = analyze(&frames(&[100; 75]), &frames(&[110; 150])).expect("calibration");
        assert_eq!(result.reason, CalibrationReason::NoSpeech);
        assert_eq!(result.confidence, CalibrationConfidence::Low);
        assert!(!result.can_apply);
    }

    #[test]
    fn rejects_clipped_speech() {
        let result = analyze(&frames(&[100; 75]), &frames(&[32_767; 150])).expect("calibration");
        assert_eq!(result.reason, CalibrationReason::Clipping);
        assert!(!result.can_apply);
        assert!(result.clipping_ratio > 0.5);
    }

    #[test]
    fn rejects_unstable_quiet_phase() {
        let mut noise = Vec::new();
        noise.extend([100; 60]);
        noise.extend([1_200; 15]);
        let result = analyze(&frames(&noise), &frames(&[2_000; 150])).expect("calibration");
        assert_eq!(result.reason, CalibrationReason::UnstableNoise);
        assert!(!result.can_apply);
    }

    #[test]
    fn requires_half_a_second_per_phase() {
        let error = analyze(&frames(&[100; 24]), &frames(&[2_000; 25])).expect_err("short phase");
        assert_eq!(error.to_string(), "calibration phases are too short");
    }
}
