//! Dependency-free stateful energy VAD for 16 kHz mono signed 16-bit PCM.

use std::fs;
use std::io;
use std::path::Path;

/// One 20 ms frame of 16 kHz mono signed 16-bit PCM.
pub const BYTES_PER_FRAME: usize = 640;
/// Audio retained before the first confirmed speech frame.
pub const PRE_ROLL_FRAMES: u32 = 8;
/// Audio retained after the last voiced frame.
pub const POST_ROLL_FRAMES: u32 = 15;

const RELEASE_FRAMES: u32 = 3;
const HANGOVER_FRAMES: u32 = 12;
const END_SILENCE_FRAMES: u32 = RELEASE_FRAMES + HANGOVER_FRAMES;

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

/// A speech-boundary event emitted while processing a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VadEvent {
    /// Speech began at the given frame, including the frames accumulated during attack.
    SpeechStarted { frame: u32 },
    /// Speech ended at the given exclusive frame after release and hangover elapsed.
    SpeechEnded { frame: u32 },
}

/// Per-frame VAD state suitable for diagnostics and a live recording indicator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VadFrameAnalysis {
    pub frame: u32,
    pub rms: u16,
    pub noise_floor: u16,
    pub adaptive_threshold: u16,
    pub voiced: bool,
    pub speech_active: bool,
    pub attack_frames: u32,
    pub release_frames: u32,
    pub hangover_frames_remaining: u32,
    pub event: Option<VadEvent>,
}

/// Stateful 20 ms-frame VAD with adaptive non-speech noise tracking.
///
/// The detector uses an entry threshold derived from the configured floor and
/// the tracked room noise. Once speech is active, a lower exit threshold avoids
/// chopping quieter phonemes. Noise is updated only while speech is inactive
/// and a frame is below the entry threshold, so confirmed speech never teaches
/// the estimator that a voice is background noise.
#[derive(Clone, Debug)]
pub struct StreamingVad {
    config: VadConfig,
    noise_floor: Option<u16>,
    adaptive_threshold: u16,
    total_frames: u32,
    voiced_frames: u32,
    rms_sum: u64,
    peak: u16,
    attack_frames: u32,
    attack_start_frame: Option<u32>,
    release_frames: u32,
    speech_active: bool,
    speech_start_frame: Option<u32>,
    speech_end_frame: Option<u32>,
}

impl StreamingVad {
    #[must_use]
    pub fn new(config: VadConfig) -> Self {
        Self::with_optional_noise_floor(config, None)
    }

    /// Creates a detector with a previously calibrated noise floor.
    #[must_use]
    pub fn with_noise_floor(config: VadConfig, noise_floor: u16) -> Self {
        Self::with_optional_noise_floor(config, Some(noise_floor))
    }

    fn with_optional_noise_floor(config: VadConfig, noise_floor: Option<u16>) -> Self {
        Self {
            config,
            noise_floor,
            adaptive_threshold: threshold_for(config, noise_floor.unwrap_or_default()),
            total_frames: 0,
            voiced_frames: 0,
            rms_sum: 0,
            peak: 0,
            attack_frames: 0,
            attack_start_frame: None,
            release_frames: 0,
            speech_active: false,
            speech_start_frame: None,
            speech_end_frame: None,
        }
    }

    /// Processes one PCM frame. A final short frame is accepted; an odd trailing
    /// byte is ignored.
    #[must_use]
    pub fn process_frame(&mut self, pcm: &[u8]) -> VadFrameAnalysis {
        let (rms, peak) = frame_level(pcm);
        self.peak = self.peak.max(peak);
        self.process_level(rms)
    }

    /// Returns accumulated stream statistics and padded trim boundaries.
    #[must_use]
    pub fn finish(self) -> VadResult {
        if self.total_frames == 0 {
            return empty_result(self.peak);
        }
        let average_rms =
            u16::try_from(self.rms_sum / u64::from(self.total_frames)).unwrap_or(u16::MAX);
        let trim_start_frame = self
            .speech_start_frame
            .map(|value| value.saturating_sub(PRE_ROLL_FRAMES));
        let trim_end_frame = self.speech_end_frame.map(|value| {
            value
                .saturating_add(POST_ROLL_FRAMES)
                .min(self.total_frames)
        });
        VadResult {
            speech_detected: self.speech_start_frame.is_some() && self.speech_end_frame.is_some(),
            voiced_frames: self.voiced_frames,
            total_frames: self.total_frames,
            peak: self.peak,
            average_rms,
            noise_floor: self.noise_floor.unwrap_or_default(),
            adaptive_threshold: self.adaptive_threshold,
            speech_start_frame: self.speech_start_frame,
            speech_end_frame: self.speech_end_frame,
            trim_start_frame,
            trim_end_frame,
        }
    }

    fn process_level(&mut self, rms: u16) -> VadFrameAnalysis {
        let frame = self.total_frames;
        self.total_frames = self.total_frames.saturating_add(1);
        self.rms_sum = self.rms_sum.saturating_add(u64::from(rms));

        let entry_threshold = self.adaptive_threshold;
        let exit_threshold = entry_threshold.saturating_mul(3) / 4;
        let mut voiced = false;
        let mut event = None;
        let mut reported_attack = 0;
        let mut reported_release = 0;

        if self.speech_active {
            voiced = rms >= exit_threshold;
            if voiced {
                self.voiced_frames = self.voiced_frames.saturating_add(1);
                self.release_frames = 0;
                self.speech_end_frame = Some(frame.saturating_add(1));
            } else {
                self.release_frames = self.release_frames.saturating_add(1);
                reported_release = self.release_frames;
                if self.release_frames >= END_SILENCE_FRAMES {
                    self.speech_active = false;
                    self.release_frames = 0;
                    event = self
                        .speech_end_frame
                        .map(|frame| VadEvent::SpeechEnded { frame });
                    self.update_noise(rms);
                }
            }
        } else if rms >= entry_threshold {
            voiced = true;
            self.voiced_frames = self.voiced_frames.saturating_add(1);
            if self.attack_frames == 0 {
                self.attack_start_frame = Some(frame);
            }
            self.attack_frames = self.attack_frames.saturating_add(1);
            reported_attack = self.attack_frames;
            if self.attack_frames >= self.config.minimum_voiced_frames.max(1) {
                let start = self.attack_start_frame.unwrap_or(frame);
                self.speech_active = true;
                self.speech_start_frame.get_or_insert(start);
                self.speech_end_frame = Some(frame.saturating_add(1));
                self.attack_frames = 0;
                self.attack_start_frame = None;
                event = Some(VadEvent::SpeechStarted { frame: start });
            }
        } else {
            self.attack_frames = 0;
            self.attack_start_frame = None;
            self.update_noise(rms);
        }

        let hangover_frames_remaining = if self.speech_active && reported_release > 0 {
            END_SILENCE_FRAMES.saturating_sub(reported_release)
        } else {
            0
        };
        VadFrameAnalysis {
            frame,
            rms,
            noise_floor: self.noise_floor.unwrap_or_default(),
            adaptive_threshold: self.adaptive_threshold,
            voiced,
            speech_active: self.speech_active,
            attack_frames: reported_attack,
            release_frames: reported_release,
            hangover_frames_remaining,
            event,
        }
    }

    fn update_noise(&mut self, rms: u16) {
        self.noise_floor = Some(match self.noise_floor {
            None => rms,
            Some(current) if rms <= current => smooth(current, rms, 3),
            Some(current) => smooth(current, rms, 5),
        });
        self.adaptive_threshold = threshold_for(self.config, self.noise_floor.unwrap_or_default());
    }
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
    if levels.is_empty() {
        return empty_result(peak);
    }

    // Batch analysis can seed the stateful detector from the whole sample. The
    // low percentile retains the previous offline behavior while live callers
    // use `StreamingVad::new` and learn noise only from non-speech frames.
    let noise_floor = low_percentile(&levels);
    let mut vad = StreamingVad::with_noise_floor(config, noise_floor);
    for level in levels {
        let _analysis = vad.process_level(level);
    }
    vad.peak = peak;
    vad.finish()
}

fn threshold_for(config: VadConfig, noise_floor: u16) -> u16 {
    let noise_threshold = noise_floor.saturating_mul(2).saturating_add(80);
    config
        .rms_threshold
        .max(noise_threshold.min(config.rms_threshold.saturating_mul(4)))
}

fn smooth(current: u16, sample: u16, shift: u32) -> u16 {
    let current = i32::from(current);
    let delta = i32::from(sample) - current;
    u16::try_from(current + (delta >> shift)).unwrap_or_default()
}

fn low_percentile(levels: &[u16]) -> u16 {
    let mut sorted = levels.to_vec();
    sorted.sort_unstable();
    let index = sorted.len().saturating_sub(1).saturating_mul(20) / 100;
    sorted[index]
}

fn frame_levels(pcm: &[u8]) -> (Vec<u16>, u16) {
    let mut levels = Vec::with_capacity(pcm.len().div_ceil(BYTES_PER_FRAME));
    let mut peak = 0_u16;
    for frame in pcm.chunks(BYTES_PER_FRAME) {
        let (rms, frame_peak) = frame_level(frame);
        if frame.len() >= 2 {
            levels.push(rms);
            peak = peak.max(frame_peak);
        }
    }
    (levels, peak)
}

fn frame_level(frame: &[u8]) -> (u16, u16) {
    let mut square_sum = 0_u64;
    let mut samples = 0_u64;
    let mut peak = 0_u16;
    for bytes in frame.chunks_exact(2) {
        let magnitude = i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs();
        peak = peak.max(magnitude);
        let value = u64::from(magnitude);
        square_sum = square_sum.saturating_add(value.saturating_mul(value));
        samples = samples.saturating_add(1);
    }
    let rms = square_sum
        .checked_div(samples)
        .map(integer_sqrt)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or_default();
    (rms, peak)
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

    fn frame(level: i16) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(BYTES_PER_FRAME);
        for _ in 0..(BYTES_PER_FRAME / 2) {
            pcm.extend_from_slice(&level.to_le_bytes());
        }
        pcm
    }

    fn frames(levels: &[i16]) -> Vec<u8> {
        let mut pcm = Vec::new();
        for level in levels {
            pcm.extend(frame(*level));
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
    fn preserves_first_start_and_last_end_across_multiple_segments() {
        let mut levels = vec![100; 10];
        levels.extend([2_000; 5]);
        levels.extend([100; 20]);
        levels.extend([2_000; 5]);
        levels.extend([100; 2]);
        let result = analyze_pcm(&frames(&levels), CONFIG);

        assert!(result.speech_detected);
        assert_eq!(result.speech_start_frame, Some(10));
        assert_eq!(result.speech_end_frame, Some(40));
        assert_eq!(result.trim_start_frame, Some(2));
        assert_eq!(result.trim_end_frame, Some(42));
    }

    #[test]
    fn streaming_events_include_attack_release_and_hangover() {
        let mut vad = StreamingVad::with_noise_floor(CONFIG, 100);
        let mut events = Vec::new();
        let mut release_state = None;
        for level in [2_000; 5].into_iter().chain([100; 15]) {
            let analysis = vad.process_frame(&frame(level));
            if analysis.release_frames == RELEASE_FRAMES {
                release_state = Some(analysis);
            }
            if let Some(event) = analysis.event {
                events.push(event);
            }
        }

        let release_state = release_state.expect("release state is observed");
        assert!(release_state.speech_active);
        assert_eq!(release_state.hangover_frames_remaining, HANGOVER_FRAMES);
        assert_eq!(
            events,
            vec![
                VadEvent::SpeechStarted { frame: 0 },
                VadEvent::SpeechEnded { frame: 5 }
            ]
        );
        let result = vad.finish();
        assert_eq!(result.speech_start_frame, Some(0));
        assert_eq!(result.speech_end_frame, Some(5));
    }

    #[test]
    fn non_speech_noise_updates_raise_the_streaming_threshold() {
        let mut vad = StreamingVad::new(CONFIG);
        for _ in 0..20 {
            let analysis = vad.process_frame(&frame(100));
            assert!(analysis.event.is_none());
        }
        let initial_threshold = vad.adaptive_threshold;
        for _ in 0..80 {
            let analysis = vad.process_frame(&frame(220));
            assert!(analysis.event.is_none());
            assert!(!analysis.speech_active);
        }
        let result = vad.finish();

        assert_eq!(initial_threshold, 300);
        assert!(result.noise_floor > 180);
        assert!(result.adaptive_threshold > initial_threshold);
        assert!(!result.speech_detected);
    }

    #[test]
    fn transient_click_does_not_start_speech_or_raise_noise() {
        let mut vad = StreamingVad::with_noise_floor(CONFIG, 100);
        let first = vad.process_frame(&frame(2_000));
        let second = vad.process_frame(&frame(100));
        let result = vad.finish();

        assert_eq!(first.attack_frames, 1);
        assert!(first.event.is_none());
        assert_eq!(second.attack_frames, 0);
        assert!(second.event.is_none());
        assert!(!result.speech_detected);
        assert_eq!(result.noise_floor, 100);
    }

    #[test]
    fn detects_fast_speech_onset_at_stream_start() {
        let mut vad = StreamingVad::new(CONFIG);
        let first = vad.process_frame(&frame(2_000));
        let second = vad.process_frame(&frame(2_000));
        let result = vad.finish();

        assert_eq!(first.attack_frames, 1);
        assert_eq!(second.event, Some(VadEvent::SpeechStarted { frame: 0 }));
        assert_eq!(result.speech_start_frame, Some(0));
        assert_eq!(result.speech_end_frame, Some(2));
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
    fn integer_square_root_is_bounded() {
        assert_eq!(integer_sqrt(0), 0);
        assert_eq!(integer_sqrt(2_000_u64.pow(2)), 2_000);
        assert_eq!(integer_sqrt(15), 3);
    }
}
