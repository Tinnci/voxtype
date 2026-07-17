//! D-Bus daemon interface.

use crate::{
    audio::{AudioFrameMetrics, Recording, RecordingResult, cleanup_stale_recordings},
    config::{
        Config, InsertionBackend, ProfileConfig, ProviderConfig, lookup_deepgram_secret,
        lookup_secret,
    },
    desktop::ClipboardInserter,
    fcitx::FcitxBridge,
    grammar,
    vad::{self, VadConfig, VadResult},
};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::{
    Mutex,
    mpsc::{Receiver, TryRecvError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use voxtype_core::{
    AudioAcceptance, Command, CommandEffect, ErrorCategory, FallbackReason, ProviderAttemptFailure,
    ReplayPolicy, SessionId, SessionMachine, StartRequest, TriggerMode, VoxError,
    routing::may_fallback,
};
use voxtype_provider_common::CancellationToken;
use voxtype_provider_deepgram::{
    DeepgramConfig, transcribe_pcm_with_evidence as transcribe_deepgram_pcm,
};
use voxtype_provider_rest::{
    ApiUsage, RestProviderConfig, transcribe_pcm_with_evidence as transcribe_pcm,
};
use zbus::fdo;

#[derive(Debug)]
pub struct VoxTypeDaemon {
    machine: SessionMachine,
    events: VecDeque<DaemonEvent>,
    recording: Option<Recording>,
    inserter: ClipboardInserter,
    config: Config,
    active_profile: Option<String>,
    armed_insertion: Option<ArmedInsertion>,
    provider_health: std::collections::BTreeMap<String, ProviderHealthState>,
    provider_usage: std::collections::BTreeMap<String, ProviderUsageState>,
    transcript_history: VecDeque<String>,
    recording_started_at: Option<Instant>,
    recognition_job: Option<RecognitionJob>,
    live_vad: Option<vad::StreamingVad>,
    last_live_audio: Option<vad::VadFrameAnalysis>,
    last_audio_overlay_at: Option<Instant>,
    quit: bool,
}

const MAX_PENDING_EVENTS: usize = 256;
const PROVIDER_VERIFICATION_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonEvent {
    StateChanged {
        state: String,
        session: String,
    },
    SessionFinished {
        session: String,
        outcome: String,
        error_code: String,
        backend: String,
        char_count: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArmedInsertion {
    Fcitx,
    Clipboard,
    Copy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompletedInsertion {
    backend: &'static str,
    clipboard_restored: bool,
}

#[derive(Clone, Debug, Default)]
struct ProviderHealthState {
    consecutive_failures: u32,
    blocked_until: Option<Instant>,
    last_success_at: Option<Instant>,
    last_failure_at: Option<Instant>,
    last_error_category: Option<ErrorCategory>,
    last_error_code: Option<&'static str>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct ProviderUsageState {
    attempts: u64,
    requests: u64,
    successes: u64,
    failures: u64,
    audio_millis: u64,
    token_reports: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    reported_tokens: u64,
}

#[derive(Clone, Debug)]
struct ProviderTranscript {
    text: String,
    api_usage: ApiUsage,
}

#[derive(Clone, Debug)]
struct ProviderInvocationSuccess {
    transcript: ProviderTranscript,
    transport_started: bool,
    audio_acceptance: AudioAcceptance,
}

#[derive(Debug)]
enum PreparedProvider {
    Mock(String),
    Rest(RestProviderConfig),
    Deepgram(DeepgramConfig),
    Command {
        program: String,
        args: Vec<String>,
        timeout_seconds: u64,
    },
}

enum VoiceActivity {
    Continue {
        result: Option<VadResult>,
        audio_millis: u64,
    },
    NoSpeech(String),
}

#[derive(Debug)]
struct RecognitionJob {
    session: SessionId,
    cancellation: CancellationToken,
    receiver: Mutex<Receiver<RecognitionWorkerResult>>,
    recording_path: std::path::PathBuf,
    vad_result: Option<VadResult>,
    audio_millis: u64,
}

#[derive(Debug)]
struct RecognitionWorkerResult {
    attempts: Vec<ProviderAttemptReport>,
    outcome: Result<RecognitionSuccess, VoxError>,
}

#[derive(Debug)]
struct RecognitionSuccess {
    provider_id: String,
    transcript: ProviderTranscript,
}

#[derive(Debug)]
struct ProviderAttemptReport {
    provider_id: String,
    transport_started: bool,
    audio_acceptance: AudioAcceptance,
    error: Option<VoxError>,
}

impl ProviderHealthState {
    fn is_available_at(&self, now: Instant) -> bool {
        self.blocked_until.is_none_or(|deadline| now >= deadline)
    }

    fn record_success_at(&mut self, now: Instant) {
        self.last_success_at = Some(now);
        self.consecutive_failures = 0;
        self.blocked_until = None;
    }

    fn record_failure_at(&mut self, now: Instant, error: &VoxError) {
        self.last_failure_at = Some(now);
        self.last_error_category = Some(error.category());
        self.last_error_code = Some(error.code());
        if error.is_retryable() {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= 3 {
                self.blocked_until = Some(now + Duration::from_secs(60));
            }
        }
    }

    fn verified_at(&self, now: Instant) -> bool {
        self.last_success_at.is_some_and(|success| {
            now.saturating_duration_since(success) <= PROVIDER_VERIFICATION_TTL
        })
    }
}

impl ProviderUsageState {
    fn record_attempt(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    fn record_transport_started(&mut self) {
        self.requests = self.requests.saturating_add(1);
    }

    fn record_audio_exposure(&mut self, audio_millis: u64) {
        self.audio_millis = self.audio_millis.saturating_add(audio_millis);
    }

    fn record_success(&mut self, usage: ApiUsage) {
        self.successes = self.successes.saturating_add(1);
        if usage.input_tokens.is_none()
            && usage.output_tokens.is_none()
            && usage.total_tokens.is_none()
        {
            return;
        }
        self.token_reports = self.token_reports.saturating_add(1);
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.total_tokens.unwrap_or(0));
        let reported = usage.total_tokens.unwrap_or_else(|| {
            usage
                .input_tokens
                .unwrap_or(0)
                .saturating_add(usage.output_tokens.unwrap_or(0))
        });
        self.reported_tokens = self.reported_tokens.saturating_add(reported);
    }

    fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
}

#[zbus::interface(name = "io.github.tinnci.VoxType1")]
impl VoxTypeDaemon {
    fn status(&self) -> String {
        self.machine.state().name().to_owned()
    }

    fn active_session(&self) -> String {
        self.machine
            .state()
            .session()
            .map_or_else(String::new, ToString::to_string)
    }

    #[zbus(signal)]
    pub async fn state_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        state: &str,
        session: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn session_finished(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session: &str,
        outcome: &str,
        error_code: &str,
        backend: &str,
        char_count: u64,
    ) -> zbus::Result<()>;

    /// Returns a structured snapshot of configured provider runtime health.
    fn provider_status(&self) -> String {
        provider_status_json(&self.config, &self.provider_health, Instant::now())
    }

    /// Returns session-local consumption counters and configured soft limits.
    fn usage_status(&self) -> String {
        let providers = self
            .config
            .providers
            .keys()
            .map(|id| {
                let usage = self.provider_usage.get(id).cloned().unwrap_or_default();
                let quota = self.config.quotas.get(id).cloned().unwrap_or_default();
                (
                    id.clone(),
                    serde_json::json!({
                        "usage": usage,
                        "quota": quota,
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        serde_json::json!({
            "scope": "daemon-session",
            "providers": providers,
        })
        .to_string()
    }

    fn last_transcript(&self) -> String {
        self.transcript_history.back().cloned().unwrap_or_default()
    }

    fn transcript_history(&self) -> Vec<String> {
        self.transcript_history.iter().cloned().collect()
    }

    fn check_last_grammar(&self) -> fdo::Result<String> {
        let text = self
            .transcript_history
            .back()
            .map(String::as_str)
            .ok_or_else(|| fdo::Error::Failed("no previous transcript is available".to_owned()))?;
        let report = grammar::check(text);
        let body = cleanup_overlay_body(&report, None);
        overlay("grammar", "Local text cleanup", &body, 5_000);
        Ok(report.render())
    }

    #[allow(clippy::unused_self)] // Public D-Bus action intentionally has no daemon-held text state.
    fn check_context_grammar(&self) -> fdo::Result<String> {
        let context = match FcitxBridge.context() {
            Ok(context) => context,
            Err(error) => {
                overlay("error", "Focused text unavailable", error.message(), 3_000);
                return Err(map_error(error));
            }
        };
        let text = context.review_text();
        if text.trim().is_empty() {
            overlay(
                "grammar",
                "No focused text to review",
                "Select text or place the cursor after a paragraph",
                3_000,
            );
            return Err(fdo::Error::Failed(
                "the focused input context has no reviewable text".to_owned(),
            ));
        }
        let report = grammar::check(&text);
        let body = cleanup_overlay_body(&report, Some(&context.target.program));
        overlay("grammar", "Focused text cleanup", &body, 5_000);
        let mut response = serde_json::to_value(&report).map_err(|error| {
            fdo::Error::Failed(format!("could not serialize cleanup report: {error}"))
        })?;
        response["source"] = serde_json::json!({
            "kind": "fcitx",
            "program": context.target.program,
            "frontend": context.target.frontend,
            "generation": context.generation,
            "truncated": context.truncated,
        });
        Ok(response.to_string())
    }

    fn clear_history(&mut self) {
        self.transcript_history.clear();
    }

    fn start(&mut self, profile: &str) -> fdo::Result<String> {
        if self.recording.is_some() {
            return Err(fdo::Error::Failed("recording is already active".to_owned()));
        }
        let (profile_name, selected_profile) = self
            .config
            .profile((!profile.is_empty()).then_some(profile))
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown profile: {profile}")))?;
        if profile.is_empty() && profile_is_demo_only(&self.config, selected_profile) {
            return Err(fdo::Error::Failed(
                "the default profile only contains fixed-text demo providers; configure a real ASR provider in VoxType Settings, or pass the demo profile explicitly for integration testing"
                    .to_owned(),
            ));
        }
        let profile_name = profile_name.to_owned();
        let request = StartRequest {
            mode: TriggerMode::Toggle,
            profile: Some(profile_name.clone()),
        };
        let effect = self
            .apply_command(Command::Start(request))
            .map_err(map_error)?;
        let CommandEffect::BeginCapture { session, .. } = effect else {
            return Err(fdo::Error::Failed("invalid start effect".to_owned()));
        };

        let armed = match self.config.desktop.insertion_backend {
            InsertionBackend::Fcitx => FcitxBridge.arm(&session).map(|()| ArmedInsertion::Fcitx),
            InsertionBackend::Clipboard => Ok(ArmedInsertion::Clipboard),
            InsertionBackend::Copy => Ok(ArmedInsertion::Copy),
            InsertionBackend::Auto => select_auto_insertion(FcitxBridge.arm(&session)),
        };
        match armed {
            Ok(armed) => self.armed_insertion = Some(armed),
            Err(error) => {
                let _effect = self.apply_command(Command::Fail {
                    session,
                    error: error.clone(),
                });
                return Err(map_error(error));
            }
        }

        match Recording::start_with_device(Some(self.config.audio.device.as_str())) {
            Ok(recording) => {
                self.recording = Some(recording);
                self.live_vad = self.config.audio.vad_enabled.then(|| {
                    vad::StreamingVad::new(VadConfig {
                        rms_threshold: self.config.audio.vad_rms_threshold,
                        minimum_voiced_frames: self.config.audio.vad_minimum_voiced_frames,
                    })
                });
                self.last_live_audio = None;
                self.last_audio_overlay_at = None;
                self.recording_started_at = Some(Instant::now());
                self.active_profile = Some(profile_name);
                self.apply_command(Command::CaptureReady {
                    session: session.clone(),
                })
                .map_err(map_error)?;
                notify("VoxType", "Listening…");
                overlay(
                    "listening",
                    "Listening",
                    "Speak now · shortcut again to stop",
                    0,
                );
                Ok(session.to_string())
            }
            Err(error) => {
                if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
                    FcitxBridge.cancel(&session);
                }
                self.armed_insertion = None;
                let domain_error = voxtype_core::VoxError::new(
                    voxtype_core::ErrorCategory::Unavailable,
                    "audio.start_failed",
                    error.to_string(),
                );
                let _effect = self.apply_command(Command::Fail {
                    session,
                    error: domain_error,
                });
                Err(fdo::Error::Failed(format!("audio capture failed: {error}")))
            }
        }
    }

    fn stop(&mut self, session: &str) -> fdo::Result<String> {
        let active = self.active_session_id(session)?;
        self.poll_audio_telemetry();
        self.apply_command(Command::Stop {
            session: active.clone(),
        })
        .map_err(map_error)?;
        let recording = self
            .recording
            .take()
            .ok_or_else(|| fdo::Error::Failed("recording process is missing".to_owned()))?;
        self.recording_started_at = None;
        self.live_vad = None;
        self.last_live_audio = None;
        self.last_audio_overlay_at = None;
        let result = match recording.stop() {
            Ok(result) => result,
            Err(error) => {
                let domain_error = VoxError::new(
                    ErrorCategory::Unavailable,
                    "audio.stop_failed",
                    error.to_string(),
                );
                let _effect = self.apply_command(Command::Fail {
                    session: active,
                    error: domain_error,
                });
                return Err(fdo::Error::Failed(format!(
                    "failed to stop capture: {error}"
                )));
            }
        };
        overlay(
            "processing",
            "Processing speech",
            "Running VAD and recognition",
            0,
        );
        let response = self.begin_recognition(&active, &result);
        if let Err(error) = &response {
            self.fail_nonterminal_session(
                &active,
                "recognition.pipeline_failed",
                error.to_string(),
            );
        }
        if response.is_err() && self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(&active);
        }
        if self.recognition_job.is_none() && !self.config.desktop.retain_recordings {
            let _result = fs::remove_file(&result.path);
        }
        if self.recognition_job.is_none() {
            self.active_profile = None;
            self.armed_insertion = None;
        }
        response
    }

    fn toggle(&mut self, profile: &str) -> fdo::Result<String> {
        if self.recording.is_some() {
            self.stop("")
        } else {
            self.start(profile)
        }
    }

    fn cancel(&mut self, session: &str) -> fdo::Result<()> {
        let active = self.active_session_id(session)?;
        self.apply_command(Command::Cancel {
            session: active.clone(),
        })
        .map_err(map_error)?;
        if let Some(recording) = self.recording.take() {
            recording.cancel();
        }
        if let Some(job) = self.recognition_job.take() {
            job.cancellation.cancel();
            let _ = fs::remove_file(&job.recording_path);
        }
        self.recording_started_at = None;
        self.live_vad = None;
        self.last_live_audio = None;
        self.last_audio_overlay_at = None;
        if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(&active);
        }
        self.active_profile = None;
        self.armed_insertion = None;
        notify("VoxType", "Dictation cancelled");
        overlay(
            "cancelled",
            "Dictation cancelled",
            "No text was inserted",
            1_800,
        );
        Ok(())
    }

    fn reset(&mut self) -> fdo::Result<()> {
        self.apply_command(Command::Reset).map_err(map_error)?;
        Ok(())
    }

    fn reload_configuration(&mut self) -> fdo::Result<()> {
        if self.recording.is_some() || self.recognition_job.is_some() {
            return Err(fdo::Error::Failed(
                "configuration reload requires an idle daemon".to_owned(),
            ));
        }
        let config = Config::load_or_create().map_err(map_error)?;
        self.inserter = ClipboardInserter::default().with_restore(config.desktop.restore_clipboard);
        self.config = config;
        if !self.config.desktop.transcript_history_enabled {
            self.transcript_history.clear();
        }
        self.provider_health.clear();
        Ok(())
    }

    fn insert_test(&self, text: &str) -> fdo::Result<String> {
        let result = self
            .inserter
            .insert(text)
            .map_err(|error| fdo::Error::Failed(format!("text insertion failed: {error}")))?;
        Ok(format!(
            "backend={} clipboard_restored={}",
            result.backend, result.clipboard_restored
        ))
    }

    fn quit(&mut self) {
        if let Some(recording) = self.recording.take() {
            recording.cancel();
        }
        if let Some(job) = self.recognition_job.take() {
            job.cancellation.cancel();
            let _ = fs::remove_file(&job.recording_path);
        }
        self.recording_started_at = None;
        self.live_vad = None;
        self.last_live_audio = None;
        self.last_audio_overlay_at = None;
        if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            if let Some(session) = self.machine.state().session() {
                FcitxBridge.cancel(session);
            }
        }
        self.quit = true;
    }

    #[zbus(property)]
    fn should_quit(&self) -> bool {
        self.quit
    }
}

impl VoxTypeDaemon {
    /// Loads configuration and constructs the D-Bus service.
    ///
    /// # Errors
    ///
    /// Returns a normalized configuration error if startup configuration cannot
    /// be created, parsed, or validated.
    pub fn load() -> Result<Self, VoxError> {
        let config = Config::load_or_create()?;
        if !config.desktop.retain_recordings {
            cleanup_stale_recordings();
        }
        let inserter = ClipboardInserter::default().with_restore(config.desktop.restore_clipboard);
        Ok(Self {
            machine: SessionMachine::default(),
            events: VecDeque::from([DaemonEvent::StateChanged {
                state: "idle".to_owned(),
                session: String::new(),
            }]),
            recording: None,
            inserter,
            config,
            active_profile: None,
            armed_insertion: None,
            provider_health: std::collections::BTreeMap::new(),
            provider_usage: std::collections::BTreeMap::new(),
            transcript_history: VecDeque::with_capacity(20),
            recording_started_at: None,
            recognition_job: None,
            live_vad: None,
            last_live_audio: None,
            last_audio_overlay_at: None,
            quit: false,
        })
    }

    #[must_use]
    pub const fn should_quit_value(&self) -> bool {
        self.quit
    }

    #[must_use]
    pub fn state_snapshot(&self) -> (String, String) {
        (
            self.machine.state().name().to_owned(),
            self.machine
                .state()
                .session()
                .map_or_else(String::new, ToString::to_string),
        )
    }

    #[must_use]
    pub fn drain_events(&mut self) -> Vec<DaemonEvent> {
        self.events.drain(..).collect()
    }

    fn apply_command(&mut self, command: Command) -> Result<CommandEffect, VoxError> {
        if self.events.len() > MAX_PENDING_EVENTS.saturating_sub(2) {
            return Err(VoxError::new(
                ErrorCategory::Unavailable,
                "daemon.event_backpressure",
                "desktop event queue is temporarily full",
            )
            .with_retryable(true));
        }
        let terminal = match &command {
            Command::Cancel { session } => Some((session.clone(), "cancelled", "")),
            Command::NoSpeech { session } => Some((session.clone(), "no-speech", "")),
            Command::Fail { session, error } => Some((session.clone(), "failed", error.code())),
            _ => None,
        };
        let effect = self.machine.apply(command)?;
        self.queue_current_state();
        if let Some((session, outcome, error_code)) = terminal {
            self.queue_session_finished(&session, outcome, error_code, "", 0);
        }
        Ok(effect)
    }

    fn queue_current_state(&mut self) {
        self.events.push_back(DaemonEvent::StateChanged {
            state: self.machine.state().name().to_owned(),
            session: self
                .machine
                .state()
                .session()
                .map_or_else(String::new, ToString::to_string),
        });
    }

    fn queue_session_finished(
        &mut self,
        session: &SessionId,
        outcome: &str,
        error_code: &str,
        backend: &str,
        char_count: u64,
    ) {
        debug_assert!(self.events.len() < MAX_PENDING_EVENTS);
        self.events.push_back(DaemonEvent::SessionFinished {
            session: session.to_string(),
            outcome: outcome.to_owned(),
            error_code: error_code.to_owned(),
            backend: backend.to_owned(),
            char_count,
        });
    }

    /// Stops a recording that exceeded the configured safety duration.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if stopping or recognition fails.
    pub fn enforce_recording_deadline(&mut self) -> fdo::Result<bool> {
        let Some(started_at) = self.recording_started_at else {
            return Ok(false);
        };
        if !recording_deadline_reached(
            started_at.elapsed(),
            self.config.audio.maximum_duration_seconds,
        ) {
            return Ok(false);
        }
        overlay(
            "processing",
            "Maximum duration reached",
            "Stopping safely and processing captured speech",
            0,
        );
        self.stop("")?;
        Ok(true)
    }

    /// Applies a completed background recognition result, if one is ready.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the provider failed or final insertion could
    /// not be completed. Late results for another/cancelled session are dropped.
    pub fn poll_recognition(&mut self) -> fdo::Result<bool> {
        let received = match self.recognition_job.as_ref() {
            None => return Ok(false),
            Some(job) => match job
                .receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv()
            {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => RecognitionWorkerResult {
                    attempts: Vec::new(),
                    outcome: Err(VoxError::new(
                        ErrorCategory::Internal,
                        "provider.worker_disconnected",
                        "recognition worker exited without a result",
                    )),
                },
            },
        };
        let Some(job) = self.recognition_job.take() else {
            return Ok(false);
        };
        let is_current = self.machine.state().name() == "finalizing"
            && self.machine.state().session() == Some(&job.session);
        if !is_current {
            self.cleanup_recognition_job(&job);
            return Ok(true);
        }

        for attempt in received.attempts {
            let usage = self
                .provider_usage
                .entry(attempt.provider_id.clone())
                .or_default();
            usage.record_attempt();
            if attempt.transport_started {
                usage.record_transport_started();
            }
            if attempt.audio_acceptance.may_have_left_process() {
                usage.record_audio_exposure(job.audio_millis);
            }
            if let Some(error) = attempt.error {
                if error.category() == ErrorCategory::Cancelled {
                    continue;
                }
                usage.record_failure();
                self.provider_failed(&attempt.provider_id, &error);
            }
        }

        let result = match received.outcome {
            Ok(success) => self.complete_recognition(
                &job.session,
                &success.provider_id,
                success.transcript,
                job.vad_result,
            ),
            Err(error) => self.fail_recognition(&job.session, error),
        };
        if result.is_err() && self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(&job.session);
        }
        self.cleanup_recognition_job(&job);
        self.active_profile = None;
        self.armed_insertion = None;
        result.map(|_| true)
    }

    /// Consumes bounded capture telemetry and updates the listening overlay.
    ///
    /// Only levels, clipping counts, and VAD state are sent to the overlay;
    /// PCM and transcript data never leave the daemon through this path.
    pub fn poll_audio_telemetry(&mut self) {
        let frames = self
            .recording
            .as_mut()
            .map(Recording::drain_frames)
            .unwrap_or_default();
        if frames.is_empty() {
            return;
        }
        let Some(live_vad) = self.live_vad.as_mut() else {
            return;
        };
        let mut latest = None;
        let mut clipped = 0_u32;
        let mut samples = 0_u32;
        for AudioFrameMetrics {
            rms,
            clipped_samples,
            samples: frame_samples,
            ..
        } in frames
        {
            latest = Some(live_vad.process_level(rms));
            clipped = clipped.saturating_add(u32::from(clipped_samples));
            samples = samples.saturating_add(u32::from(frame_samples));
        }
        let Some(analysis) = latest else {
            return;
        };
        self.last_live_audio = Some(analysis);
        let now = Instant::now();
        if self
            .last_audio_overlay_at
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(100))
        {
            return;
        }
        self.last_audio_overlay_at = Some(now);
        let clip_percent = clipped
            .saturating_mul(100)
            .checked_div(samples)
            .unwrap_or_default();
        overlay_audio_metrics(analysis, clip_percent);
    }

    fn cleanup_recognition_job(&self, job: &RecognitionJob) {
        if !self.config.desktop.retain_recordings {
            let _ = fs::remove_file(&job.recording_path);
        }
    }

    fn fail_nonterminal_session(
        &mut self,
        session: &SessionId,
        code: &'static str,
        message: String,
    ) {
        let is_active = self.machine.state().session() == Some(session)
            && matches!(
                self.machine.state().name(),
                "preparing" | "listening" | "finalizing" | "inserting"
            );
        if is_active {
            let _effect = self.apply_command(Command::Fail {
                session: session.clone(),
                error: VoxError::new(ErrorCategory::Internal, code, message),
            });
        }
    }

    fn active_session_id(&self, requested: &str) -> fdo::Result<SessionId> {
        let active = self
            .machine
            .state()
            .session()
            .cloned()
            .ok_or_else(|| fdo::Error::Failed("no active session".to_owned()))?;
        if requested.is_empty() || requested == active.as_str() {
            Ok(active)
        } else {
            Err(fdo::Error::InvalidArgs(
                "session ID does not match".to_owned(),
            ))
        }
    }

    fn begin_recognition(
        &mut self,
        session: &SessionId,
        recording: &RecordingResult,
    ) -> fdo::Result<String> {
        if let Some(response) = self.check_minimum_duration(session, recording)? {
            return Ok(response);
        }

        let (vad_result, audio_millis) = match self.check_voice_activity(session, recording)? {
            VoiceActivity::Continue {
                result,
                audio_millis,
            } => (result, audio_millis),
            VoiceActivity::NoSpeech(response) => return Ok(response),
        };

        let profile_name = self
            .active_profile
            .as_deref()
            .unwrap_or(&self.config.default_profile);
        let profile = self
            .config
            .profiles
            .get(profile_name)
            .ok_or_else(|| fdo::Error::Failed("active profile disappeared".to_owned()))?;
        let providers = std::iter::once(&profile.primary)
            .chain(profile.fallbacks.iter())
            .cloned()
            .collect::<Vec<_>>();
        let replay = ReplayPolicy::from(profile.replay);
        let language = profile.language.clone();

        let provider_configs = providers
            .into_iter()
            .filter(|provider_id| self.provider_is_available(provider_id))
            .filter_map(|provider_id| {
                self.config
                    .providers
                    .get(&provider_id)
                    .cloned()
                    .map(|config| (provider_id, config))
            })
            .collect::<Vec<_>>();
        if provider_configs.is_empty() {
            return self.fail_recognition(
                session,
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.no_route",
                    "no configured provider is currently available",
                ),
            );
        }

        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let path = recording.path.clone();
        let worker_path = path.clone();
        let (sender, receiver) = sync_channel(1);
        thread::Builder::new()
            .name(format!("voxtype-recognition-{session}"))
            .spawn(move || {
                let result = run_recognition_worker(
                    provider_configs,
                    replay,
                    &worker_path,
                    &language,
                    &worker_cancellation,
                );
                let _ = sender.send(result);
            })
            .map_err(|error| fdo::Error::Failed(format!("could not start recognition: {error}")))?;
        self.recognition_job = Some(RecognitionJob {
            session: session.clone(),
            cancellation,
            receiver: Mutex::new(receiver),
            recording_path: path,
            vad_result,
            audio_millis,
        });
        Ok(format!("session={session} result=processing"))
    }

    fn fail_recognition(&mut self, session: &SessionId, error: VoxError) -> fdo::Result<String> {
        let message = error.to_string();
        let _effect = self.apply_command(Command::Fail {
            session: session.clone(),
            error,
        });
        notify("VoxType recognition failed", &message);
        overlay(
            "error",
            "Recognition failed",
            "Open diagnostics for details",
            3_500,
        );
        Err(fdo::Error::Failed(message))
    }

    fn complete_recognition(
        &mut self,
        session: &SessionId,
        provider_id: &str,
        transcript: ProviderTranscript,
        vad_result: Option<VadResult>,
    ) -> fdo::Result<String> {
        let ProviderTranscript { text, api_usage } = transcript;
        self.provider_usage
            .entry(provider_id.to_owned())
            .or_default()
            .record_success(api_usage);
        self.provider_succeeded(provider_id);
        let effect = self
            .apply_command(Command::TranscriptReady {
                session: session.clone(),
                text: text.clone(),
            })
            .map_err(map_error)?;
        let CommandEffect::InsertText { .. } = effect else {
            return Err(fdo::Error::Failed(
                "state machine did not request insertion".to_owned(),
            ));
        };
        let insertion = match self.insert_text(session, &text) {
            Ok(insertion) => insertion,
            Err(error) => {
                let mapped = map_error(error.clone());
                let _effect = self.apply_command(Command::Fail {
                    session: session.clone(),
                    error,
                });
                return Err(mapped);
            }
        };
        self.apply_command(Command::InsertionComplete {
            session: session.clone(),
        })
        .map_err(map_error)?;
        self.queue_session_finished(
            session,
            "completed",
            "",
            insertion.backend,
            u64::try_from(text.chars().count()).unwrap_or(u64::MAX),
        );
        if self.config.desktop.transcript_history_enabled {
            if self.transcript_history.len() == 20 {
                self.transcript_history.pop_front();
            }
            self.transcript_history.push_back(text.clone());
        }
        if insertion.backend == "copy-only" {
            notify("VoxType", "Dictation copied to clipboard");
            overlay(
                "done",
                "Text copied",
                "Paste it when the intended field is ready",
                3_000,
            );
        } else {
            notify("VoxType", "Dictation dispatched");
            let detail = "Sent to the focused input path · Meta+Alt+G reviews focused text";
            overlay("done", "Text dispatched", detail, 2_000);
        }
        Ok(format!(
            "session={session} provider={provider_id} chars={} backend={} clipboard_restored={} vad={}",
            text.chars().count(),
            insertion.backend,
            insertion.clipboard_restored,
            vad_result.map_or_else(
                || "disabled".to_owned(),
                |result| format!(
                    "speech:{}/{}:rms={}:noise={}:threshold={}:trim={}-{}:peak={}",
                    result.voiced_frames,
                    result.total_frames,
                    result.average_rms,
                    result.noise_floor,
                    result.adaptive_threshold,
                    result.trim_start_frame.unwrap_or_default(),
                    result.trim_end_frame.unwrap_or_default(),
                    result.peak
                )
            )
        ))
    }

    fn check_voice_activity(
        &mut self,
        session: &SessionId,
        recording: &RecordingResult,
    ) -> fdo::Result<VoiceActivity> {
        if !self.config.audio.vad_enabled {
            return Ok(VoiceActivity::Continue {
                result: None,
                audio_millis: recording.duration_millis,
            });
        }
        let result = vad::analyze_file(
            &recording.path,
            VadConfig {
                rms_threshold: self.config.audio.vad_rms_threshold,
                minimum_voiced_frames: self.config.audio.vad_minimum_voiced_frames,
            },
        )
        .map_err(|error| fdo::Error::Failed(format!("VAD analysis failed: {error}")))?;
        if result.speech_detected {
            let trimmed_bytes = vad::trim_file(&recording.path, &result)
                .map_err(|error| fdo::Error::Failed(format!("audio trim failed: {error}")))?;
            return Ok(VoiceActivity::Continue {
                result: Some(result),
                audio_millis: trimmed_bytes.saturating_mul(1_000) / 32_000,
            });
        }
        if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(session);
        }
        self.apply_command(Command::NoSpeech {
            session: session.clone(),
        })
        .map_err(map_error)?;
        let (reason, guidance) = no_speech_guidance(
            &result,
            self.config.audio.vad_rms_threshold,
            self.config.audio.vad_minimum_voiced_frames,
        );
        notify("VoxType", "No speech detected");
        overlay("no-speech", "No speech detected", guidance, 2_800);
        Ok(VoiceActivity::NoSpeech(format!(
            "session={session} result=no-speech reason={reason} vad_voiced_frames={} vad_total_frames={} average_rms={} noise_floor={} threshold={} peak={}",
            result.voiced_frames,
            result.total_frames,
            result.average_rms,
            result.noise_floor,
            result.adaptive_threshold,
            result.peak
        )))
    }

    fn check_minimum_duration(
        &mut self,
        session: &SessionId,
        recording: &RecordingResult,
    ) -> fdo::Result<Option<String>> {
        if recording.duration_millis >= self.config.audio.minimum_duration_millis {
            return Ok(None);
        }
        if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(session);
        }
        self.apply_command(Command::NoSpeech {
            session: session.clone(),
        })
        .map_err(map_error)?;
        notify("VoxType", "Recording was too short");
        overlay(
            "no-speech",
            "Recording too short",
            "Hold the shortcut a little longer",
            2_000,
        );
        Ok(Some(format!(
            "session={session} result=no-speech duration_ms={}",
            recording.duration_millis
        )))
    }

    fn provider_is_available(&self, provider_id: &str) -> bool {
        self.provider_health
            .get(provider_id)
            .is_none_or(|health| health.is_available_at(Instant::now()))
    }

    fn provider_succeeded(&mut self, provider_id: &str) {
        self.provider_health
            .entry(provider_id.to_owned())
            .or_default()
            .record_success_at(Instant::now());
    }

    fn provider_failed(&mut self, provider_id: &str, error: &VoxError) {
        let health = self
            .provider_health
            .entry(provider_id.to_owned())
            .or_default();
        health.record_failure_at(Instant::now(), error);
    }
}

fn cleanup_overlay_body(report: &grammar::GrammarReport, source: Option<&str>) -> String {
    let prefix = source
        .filter(|value| !value.is_empty())
        .map_or_else(String::new, |value| format!("{value} · "));
    if report.is_clean() {
        return format!("{prefix}no local cleanup suggestions");
    }
    format!(
        "{prefix}{} safe · {} need review",
        report.safe_edit_count, report.review_edit_count
    )
}

const fn error_category_name(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::InvalidArgument => "invalid-argument",
        ErrorCategory::InvalidState => "invalid-state",
        ErrorCategory::Configuration => "configuration",
        ErrorCategory::Authentication => "authentication",
        ErrorCategory::Permission => "permission",
        ErrorCategory::Connection => "connection",
        ErrorCategory::Timeout => "timeout",
        ErrorCategory::Protocol => "protocol",
        ErrorCategory::RateLimited => "rate-limited",
        ErrorCategory::Unavailable => "unavailable",
        ErrorCategory::Cancelled => "cancelled",
        ErrorCategory::Internal => "internal",
    }
}

fn provider_status_json(
    config: &Config,
    states: &std::collections::BTreeMap<String, ProviderHealthState>,
    now: Instant,
) -> String {
    let providers = config
        .providers
        .keys()
        .map(|id| {
            let state = states.get(id);
            let route_available = state.is_none_or(|health| health.is_available_at(now));
            let verified = state.is_some_and(|health| health.verified_at(now));
            let age = |instant: Option<Instant>| {
                instant.map(|value| now.saturating_duration_since(value).as_secs())
            };
            let retry_after_seconds = state
                .and_then(|health| health.blocked_until)
                .and_then(|deadline| deadline.checked_duration_since(now))
                .map(|duration| duration.as_secs());
            let health = serde_json::json!({
                "route_available": route_available,
                "verified": verified,
                "verified_age_seconds": state.and_then(|health| age(health.last_success_at)),
                "verification_ttl_seconds": PROVIDER_VERIFICATION_TTL.as_secs(),
                "consecutive_failures": state.map_or(0, |health| health.consecutive_failures),
                "retry_after_seconds": retry_after_seconds,
                "last_failure_age_seconds": state.and_then(|health| age(health.last_failure_at)),
                "last_error_category": state
                    .and_then(|health| health.last_error_category)
                    .map(error_category_name),
                "last_error_code": state.and_then(|health| health.last_error_code),
            });
            (id.clone(), health)
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    serde_json::json!({
        "schema": 1,
        "providers": providers,
    })
    .to_string()
}

fn run_recognition_worker(
    providers: Vec<(String, ProviderConfig)>,
    replay: ReplayPolicy,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> RecognitionWorkerResult {
    let mut attempts = Vec::new();
    let mut last_error = None;
    for (provider_id, config) in providers {
        if cancellation.is_cancelled() {
            last_error = Some(cancelled_error());
            break;
        }
        let prepared = match prepare_provider(&config) {
            Ok(prepared) => prepared,
            Err(error) => {
                let retryable = error.is_retryable();
                attempts.push(ProviderAttemptReport {
                    provider_id,
                    transport_started: false,
                    audio_acceptance: AudioAcceptance::NotAccepted,
                    error: Some(error.clone()),
                });
                last_error = Some(error);
                if !retryable {
                    break;
                }
                continue;
            }
        };
        match invoke_provider(prepared, pcm_path, language, cancellation) {
            Ok(success) => {
                attempts.push(ProviderAttemptReport {
                    provider_id: provider_id.clone(),
                    transport_started: success.transport_started,
                    audio_acceptance: success.audio_acceptance,
                    error: None,
                });
                return RecognitionWorkerResult {
                    attempts,
                    outcome: Ok(RecognitionSuccess {
                        provider_id,
                        transcript: success.transcript,
                    }),
                };
            }
            Err(failure) => {
                let ProviderAttemptFailure {
                    error,
                    transport_started,
                    audio_acceptance,
                } = failure;
                let retryable = error.is_retryable();
                let cancelled = error.category() == ErrorCategory::Cancelled;
                let fallback_allowed = fallback_reason(error.category())
                    .is_some_and(|reason| may_fallback(reason, audio_acceptance, replay));
                attempts.push(ProviderAttemptReport {
                    provider_id,
                    transport_started,
                    audio_acceptance,
                    error: Some(error.clone()),
                });
                last_error = Some(error);
                if cancelled || !retryable || !fallback_allowed {
                    break;
                }
            }
        }
    }
    RecognitionWorkerResult {
        attempts,
        outcome: Err(last_error.unwrap_or_else(|| {
            VoxError::new(
                ErrorCategory::Unavailable,
                "provider.no_route",
                "no provider was attempted",
            )
        })),
    }
}

fn prepare_provider(provider: &ProviderConfig) -> Result<PreparedProvider, VoxError> {
    match provider {
        ProviderConfig::Mock { text } => {
            if text.trim().is_empty() {
                Err(VoxError::new(
                    ErrorCategory::Protocol,
                    "provider.mock_empty",
                    "mock provider text is empty",
                ))
            } else {
                Ok(PreparedProvider::Mock(text.clone()))
            }
        }
        ProviderConfig::OpenaiCompatible {
            endpoint,
            model,
            secret,
            timeout_seconds,
        } => {
            let api_key = lookup_secret(secret)?;
            Ok(PreparedProvider::Rest(RestProviderConfig {
                endpoint: endpoint.clone(),
                model: model.clone(),
                api_key,
                timeout_seconds: *timeout_seconds,
            }))
        }
        ProviderConfig::Deepgram {
            endpoint,
            model,
            secret,
            timeout_seconds,
            smart_format,
        } => {
            let api_key = lookup_deepgram_secret(secret)?;
            Ok(PreparedProvider::Deepgram(DeepgramConfig {
                endpoint: endpoint.clone(),
                model: model.clone(),
                api_key,
                timeout_seconds: *timeout_seconds,
                smart_format: *smart_format,
            }))
        }
        ProviderConfig::Command {
            program,
            args,
            timeout_seconds,
        } => Ok(PreparedProvider::Command {
            program: program.clone(),
            args: args.clone(),
            timeout_seconds: *timeout_seconds,
        }),
    }
}

fn invoke_provider(
    provider: PreparedProvider,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<ProviderInvocationSuccess, ProviderAttemptFailure> {
    if cancellation.is_cancelled() {
        return Err(ProviderAttemptFailure::before_transport(cancelled_error()));
    }
    match provider {
        PreparedProvider::Mock(text) => Ok(ProviderInvocationSuccess {
            transcript: ProviderTranscript {
                text,
                api_usage: ApiUsage::default(),
            },
            transport_started: false,
            audio_acceptance: AudioAcceptance::NotAccepted,
        }),
        PreparedProvider::Rest(config) => transcribe_pcm(&config, pcm_path, language, cancellation)
            .map(|result| ProviderInvocationSuccess {
                transcript: ProviderTranscript {
                    text: result.text,
                    api_usage: result.usage,
                },
                transport_started: true,
                audio_acceptance: AudioAcceptance::Accepted,
            }),
        PreparedProvider::Deepgram(config) => {
            transcribe_deepgram_pcm(&config, pcm_path, language, cancellation).map(|result| {
                ProviderInvocationSuccess {
                    transcript: ProviderTranscript {
                        text: result.text,
                        api_usage: ApiUsage::default(),
                    },
                    transport_started: true,
                    audio_acceptance: AudioAcceptance::Accepted,
                }
            })
        }
        PreparedProvider::Command {
            program,
            args,
            timeout_seconds,
        } => transcribe_command_with_evidence(
            &program,
            &args,
            timeout_seconds,
            pcm_path,
            language,
            cancellation,
        )
        .map(|text| ProviderInvocationSuccess {
            transcript: ProviderTranscript {
                text,
                api_usage: ApiUsage::default(),
            },
            transport_started: true,
            audio_acceptance: AudioAcceptance::Accepted,
        }),
    }
}

#[cfg(test)]
fn transcribe_command(
    program: &str,
    args: &[String],
    timeout_seconds: u64,
    pcm_path: &Path,
    language: &str,
) -> Result<String, VoxError> {
    transcribe_command_cancellable(
        program,
        args,
        timeout_seconds,
        pcm_path,
        language,
        &CancellationToken::new(),
    )
}

#[cfg(test)]
fn transcribe_command_cancellable(
    program: &str,
    args: &[String],
    timeout_seconds: u64,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<String, VoxError> {
    transcribe_command_with_evidence(
        program,
        args,
        timeout_seconds,
        pcm_path,
        language,
        cancellation,
    )
    .map_err(ProviderAttemptFailure::into_error)
}

fn transcribe_command_with_evidence(
    program: &str,
    args: &[String],
    timeout_seconds: u64,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<String, ProviderAttemptFailure> {
    if cancellation.is_cancelled() {
        return Err(ProviderAttemptFailure::before_transport(cancelled_error()));
    }
    let mut child = ProcessCommand::new(program)
        .args(args)
        .env("VOXTYPE_AUDIO_PATH", pcm_path)
        .env("VOXTYPE_LANGUAGE", language)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| {
            ProviderAttemptFailure::before_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_failed",
                    error.to_string(),
                )
                .with_retryable(true),
            )
        })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Internal,
                "provider.command_output",
                "command provider stdout is unavailable",
            ),
            AudioAcceptance::PossiblyAccepted,
        )
    })?;
    let output_reader = thread::spawn(move || read_command_output(stdout, 1024 * 1024));
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let status = match wait_for_command(&mut child, deadline, cancellation) {
        Ok(status) => status,
        Err(error) => {
            let _ = output_reader.join();
            return Err(error);
        }
    };
    let output = output_reader
        .join()
        .map_err(|_| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Internal,
                    "provider.command_output",
                    "command output reader panicked",
                ),
                AudioAcceptance::Accepted,
            )
        })?
        .map_err(|error| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_output",
                    error.to_string(),
                ),
                AudioAcceptance::Accepted,
            )
        })?;
    finish_command_output(status, output)
}

fn wait_for_command(
    child: &mut std::process::Child,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<std::process::ExitStatus, ProviderAttemptFailure> {
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_wait",
                    error.to_string(),
                ),
                AudioAcceptance::PossiblyAccepted,
            )
        })? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate_process_group(child);
            return Err(ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Timeout,
                    "provider.command_timeout",
                    "command provider timed out",
                )
                .with_retryable(true),
                AudioAcceptance::PossiblyAccepted,
            ));
        }
        if cancellation.is_cancelled() {
            terminate_process_group(child);
            return Err(ProviderAttemptFailure::after_transport(
                cancelled_error(),
                AudioAcceptance::PossiblyAccepted,
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finish_command_output(
    status: std::process::ExitStatus,
    output: BoundedCommandOutput,
) -> Result<String, ProviderAttemptFailure> {
    if output.overflowed {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_output_too_large",
                "command provider output exceeded 1048576 bytes",
            ),
            AudioAcceptance::Accepted,
        ));
    }
    if !status.success() {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Unavailable,
                "provider.command_exit",
                format!("command exited with {status}"),
            )
            .with_retryable(true),
            AudioAcceptance::Accepted,
        ));
    }
    let text = String::from_utf8(output.bytes).map_err(|error| {
        ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_output",
                error.to_string(),
            ),
            AudioAcceptance::Accepted,
        )
    })?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_empty",
                "command provider returned empty output",
            ),
            AudioAcceptance::Accepted,
        ));
    }
    Ok(text)
}

struct BoundedCommandOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn read_command_output(mut reader: impl Read, limit: usize) -> io::Result<BoundedCommandOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut overflowed = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let retained = limit.saturating_sub(bytes.len()).min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        overflowed |= retained < count;
    }
    Ok(BoundedCommandOutput { bytes, overflowed })
}

fn terminate_process_group(child: &mut std::process::Child) {
    let process_group = format!("-{}", child.id());
    let _ = ProcessCommand::new("kill")
        .args(["-KILL", "--", &process_group])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

fn cancelled_error() -> VoxError {
    VoxError::new(
        ErrorCategory::Cancelled,
        "provider.cancelled",
        "recognition was cancelled",
    )
}

const fn fallback_reason(category: ErrorCategory) -> Option<FallbackReason> {
    match category {
        ErrorCategory::Connection => Some(FallbackReason::Connection),
        ErrorCategory::Timeout => Some(FallbackReason::Timeout),
        ErrorCategory::RateLimited => Some(FallbackReason::RateLimited),
        ErrorCategory::Unavailable => Some(FallbackReason::Unavailable),
        _ => None,
    }
}

fn no_speech_guidance(
    result: &VadResult,
    configured_threshold: u16,
    minimum_voiced_frames: u32,
) -> (&'static str, &'static str) {
    if result.peak >= 32_000 {
        return (
            "clipping",
            "Input clipped · lower microphone gain or move slightly farther away",
        );
    }
    if result.adaptive_threshold >= configured_threshold.saturating_mul(3)
        && result.noise_floor >= configured_threshold
    {
        return (
            "high-noise",
            "Background noise is high · reduce noise or run microphone calibration",
        );
    }
    if result.peak < result.adaptive_threshold.saturating_mul(2).max(500) {
        return (
            "too-quiet",
            "Input is too quiet · speak closer or increase microphone gain",
        );
    }
    if result.voiced_frames > 0 && result.voiced_frames < minimum_voiced_frames {
        return (
            "speech-too-short",
            "Speech was too brief · hold the shortcut and speak a little longer",
        );
    }
    (
        "unconfirmed",
        "Speech was not confirmed · speak continuously and avoid keyboard noise",
    )
}

impl VoxTypeDaemon {
    fn insert_text(&self, session: &SessionId, text: &str) -> Result<CompletedInsertion, VoxError> {
        match self.armed_insertion {
            Some(ArmedInsertion::Fcitx) => {
                FcitxBridge.commit(session, text)?;
                Ok(CompletedInsertion {
                    backend: "fcitx5",
                    clipboard_restored: true,
                })
            }
            Some(ArmedInsertion::Clipboard) => {
                let result = self.inserter.insert(text).map_err(|error| {
                    VoxError::new(
                        ErrorCategory::Unavailable,
                        "desktop.insertion_failed",
                        error.to_string(),
                    )
                })?;
                Ok(CompletedInsertion {
                    backend: result.backend,
                    clipboard_restored: result.clipboard_restored,
                })
            }
            Some(ArmedInsertion::Copy) => {
                let result = self.inserter.copy(text).map_err(|error| {
                    VoxError::new(
                        ErrorCategory::Unavailable,
                        "desktop.copy_failed",
                        error.to_string(),
                    )
                })?;
                Ok(CompletedInsertion {
                    backend: result.backend,
                    clipboard_restored: result.clipboard_restored,
                })
            }
            None => Err(VoxError::new(
                ErrorCategory::InvalidState,
                "desktop.not_armed",
                "no text insertion target was armed for the session",
            )),
        }
    }
}

fn map_error(error: voxtype_core::VoxError) -> fdo::Error {
    let rendered = format!("{}: {}", error.code(), error.message());
    drop(error);
    fdo::Error::Failed(rendered)
}

fn notify(summary: &str, body: &str) {
    let _result = ProcessCommand::new("notify-send")
        .args(["--app-name=VoxType", summary, body])
        .spawn();
}

const fn recording_deadline_reached(elapsed: Duration, maximum_seconds: u64) -> bool {
    elapsed.as_secs() >= maximum_seconds
}

fn may_auto_fallback_from_fcitx(error: &VoxError) -> bool {
    matches!(
        error.code(),
        "fcitx.transport_failed" | "fcitx.runtime_unavailable"
    )
}

fn select_auto_insertion(arm_result: Result<(), VoxError>) -> Result<ArmedInsertion, VoxError> {
    match arm_result {
        Ok(()) => Ok(ArmedInsertion::Fcitx),
        Err(error) if may_auto_fallback_from_fcitx(&error) => Ok(ArmedInsertion::Copy),
        Err(error) => Err(error),
    }
}

fn profile_is_demo_only(config: &Config, profile: &ProfileConfig) -> bool {
    std::iter::once(&profile.primary)
        .chain(&profile.fallbacks)
        .all(|provider_id| {
            matches!(
                config.providers.get(provider_id),
                Some(ProviderConfig::Mock { .. })
            )
        })
}

fn overlay(state: &str, title: &str, body: &str, timeout_millis: u32) {
    let payload = serde_json::json!({
        "state": state,
        "title": title,
        "body": body,
        "timeout_ms": timeout_millis,
    })
    .to_string();
    let Ok(mut child) = ProcessCommand::new("voxtype-overlay")
        .args(["show"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
}

fn overlay_audio_metrics(analysis: vad::VadFrameAnalysis, clipping_percent: u32) {
    let payload = serde_json::json!({
        "state": "listening",
        "title": "Listening",
        "body": format!(
            "RMS {} · threshold {} · {} · clipping {}%",
            analysis.rms,
            analysis.adaptive_threshold,
            if analysis.speech_active { "speech" } else { "noise" },
            clipping_percent
        ),
        "timeout_ms": 0,
        "visible": true,
        "rms": analysis.rms,
        "adaptive_threshold": analysis.adaptive_threshold,
        "speech_active": analysis.speech_active,
        "clipping_percent": clipping_percent.min(100),
        "updated_ms": now_millis(),
    })
    .to_string();
    let _ = write_overlay_telemetry(payload.as_bytes());
}

fn write_overlay_telemetry(payload: &[u8]) -> io::Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unavailable"))?;
    let directory = runtime.join("voxtype");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let path = directory.join("overlay-state.json");
    let temporary = directory.join(format!(
        "overlay-state.telemetry-{}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = file
        .write_all(payload)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_test_daemon() -> VoxTypeDaemon {
        let config: Config = toml::from_str(
            r#"schema_version = 1
default_profile = "test"
[desktop]
[audio]
[profiles.test]
primary = "mock"
[providers.mock]
kind = "mock"
text = "test"
"#,
        )
        .expect("test config");
        VoxTypeDaemon {
            machine: SessionMachine::default(),
            events: VecDeque::from([DaemonEvent::StateChanged {
                state: "idle".to_owned(),
                session: String::new(),
            }]),
            recording: None,
            inserter: ClipboardInserter::default(),
            config,
            active_profile: None,
            armed_insertion: None,
            provider_health: std::collections::BTreeMap::new(),
            provider_usage: std::collections::BTreeMap::new(),
            transcript_history: VecDeque::new(),
            recording_started_at: None,
            recognition_job: None,
            live_vad: None,
            last_live_audio: None,
            last_audio_overlay_at: None,
            quit: false,
        }
    }

    #[test]
    fn emits_every_happy_path_state_in_order() {
        let mut daemon = event_test_daemon();
        let effect = daemon
            .apply_command(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect("start");
        let CommandEffect::BeginCapture { session, .. } = effect else {
            panic!("expected capture effect");
        };
        daemon
            .apply_command(Command::CaptureReady {
                session: session.clone(),
            })
            .expect("capture ready");
        daemon
            .apply_command(Command::Stop {
                session: session.clone(),
            })
            .expect("stop");
        daemon
            .apply_command(Command::TranscriptReady {
                session: session.clone(),
                text: "hello".to_owned(),
            })
            .expect("transcript");
        daemon
            .apply_command(Command::InsertionComplete {
                session: session.clone(),
            })
            .expect("insertion");
        daemon.queue_session_finished(&session, "completed", "", "fcitx", 5);

        let events = daemon.drain_events();
        let states = events
            .iter()
            .filter_map(|event| match event {
                DaemonEvent::StateChanged { state, .. } => Some(state.as_str()),
                DaemonEvent::SessionFinished { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            [
                "idle",
                "preparing",
                "listening",
                "finalizing",
                "inserting",
                "completed"
            ]
        );
        assert!(matches!(
            events.last(),
            Some(DaemonEvent::SessionFinished {
                outcome,
                backend,
                char_count: 5,
                ..
            }) if outcome == "completed" && backend == "fcitx"
        ));
    }

    #[test]
    fn cancellation_emits_terminal_metadata_after_state_change() {
        let mut daemon = event_test_daemon();
        let effect = daemon
            .apply_command(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect("start");
        let CommandEffect::BeginCapture { session, .. } = effect else {
            panic!("expected capture effect");
        };
        daemon
            .apply_command(Command::Cancel {
                session: session.clone(),
            })
            .expect("cancel");
        let events = daemon.drain_events();
        assert!(matches!(
            events.as_slice(),
            [
                DaemonEvent::StateChanged { state: idle, .. },
                DaemonEvent::StateChanged { state: preparing, .. },
                DaemonEvent::StateChanged { state: cancelled, .. },
                DaemonEvent::SessionFinished { outcome, .. }
            ] if idle == "idle"
                && preparing == "preparing"
                && cancelled == "cancelled"
                && outcome == "cancelled"
        ));
    }

    #[test]
    fn provider_health_blocks_after_three_retryable_failures() {
        let now = Instant::now();
        let mut health = ProviderHealthState::default();
        let error = VoxError::new(
            ErrorCategory::Connection,
            "provider.connection",
            "connection failed",
        )
        .with_retryable(true);

        health.record_failure_at(now, &error);
        health.record_failure_at(now, &error);
        assert!(health.is_available_at(now));

        health.record_failure_at(now, &error);
        assert!(!health.is_available_at(now + Duration::from_secs(59)));
        assert!(health.is_available_at(now + Duration::from_secs(60)));
    }

    #[test]
    fn provider_verification_requires_recent_real_success() {
        let now = Instant::now();
        let mut health = ProviderHealthState::default();
        assert!(!health.verified_at(now));

        health.record_success_at(now);
        assert!(health.verified_at(now + Duration::from_secs(899)));
        assert!(!health.verified_at(now + Duration::from_secs(901)));

        let error = VoxError::new(
            ErrorCategory::Authentication,
            "provider.authentication",
            "invalid credential",
        );
        health.record_failure_at(now + Duration::from_secs(2), &error);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.last_error_code, Some("provider.authentication"));
        assert!(health.is_available_at(now + Duration::from_secs(2)));
    }

    #[test]
    fn provider_status_never_calls_an_unverified_route_healthy() {
        let daemon = event_test_daemon();
        let now = Instant::now();
        let initial: serde_json::Value = serde_json::from_str(&provider_status_json(
            &daemon.config,
            &daemon.provider_health,
            now,
        ))
        .expect("provider status json");
        assert_eq!(initial["schema"], 1);
        assert_eq!(initial["providers"]["mock"]["route_available"], true);
        assert_eq!(initial["providers"]["mock"]["verified"], false);

        let mut states = std::collections::BTreeMap::new();
        let mut health = ProviderHealthState::default();
        health.record_success_at(now);
        states.insert("mock".to_owned(), health);
        let verified: serde_json::Value = serde_json::from_str(&provider_status_json(
            &daemon.config,
            &states,
            now + Duration::from_secs(1),
        ))
        .expect("verified provider status json");
        assert_eq!(verified["providers"]["mock"]["verified"], true);
        assert_eq!(
            verified["providers"]["mock"]["verification_ttl_seconds"],
            900
        );
    }

    #[test]
    fn usage_only_counts_tokens_reported_by_api() {
        let mut usage = ProviderUsageState::default();
        usage.record_attempt();
        usage.record_failure();
        assert_eq!(usage.attempts, 1);
        assert_eq!(usage.requests, 0);
        assert_eq!(usage.audio_millis, 0);

        usage.record_attempt();
        usage.record_transport_started();
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.audio_millis, 0);
        usage.record_audio_exposure(1_250);
        usage.record_success(ApiUsage::default());
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.audio_millis, 1_250);
        assert_eq!(usage.token_reports, 0);
        assert_eq!(usage.reported_tokens, 0);

        usage.record_attempt();
        usage.record_transport_started();
        usage.record_audio_exposure(750);
        usage.record_success(ApiUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            total_tokens: Some(15),
        });
        assert_eq!(usage.attempts, 3);
        assert_eq!(usage.requests, 2);
        assert_eq!(usage.successes, 2);
        assert_eq!(usage.failures, 1);
        assert_eq!(usage.audio_millis, 2_000);
        assert_eq!(usage.token_reports, 1);
        assert_eq!(usage.reported_tokens, 15);
    }

    #[test]
    fn recording_deadline_is_inclusive() {
        assert!(!recording_deadline_reached(Duration::from_secs(119), 120));
        assert!(recording_deadline_reached(Duration::from_secs(120), 120));
    }

    #[test]
    fn no_speech_guidance_uses_real_vad_metrics() {
        let result = |peak, noise_floor, threshold, voiced_frames| VadResult {
            speech_detected: false,
            voiced_frames,
            total_frames: 20,
            peak,
            average_rms: noise_floor,
            noise_floor,
            adaptive_threshold: threshold,
            speech_start_frame: None,
            speech_end_frame: None,
            trim_start_frame: None,
            trim_end_frame: None,
        };

        assert_eq!(
            no_speech_guidance(&result(32_100, 100, 300, 0), 300, 2).0,
            "clipping"
        );
        assert_eq!(
            no_speech_guidance(&result(2_000, 500, 900, 0), 300, 2).0,
            "high-noise"
        );
        assert_eq!(
            no_speech_guidance(&result(400, 100, 300, 0), 300, 2).0,
            "too-quiet"
        );
        assert_eq!(
            no_speech_guidance(&result(2_000, 100, 300, 1), 300, 2).0,
            "speech-too-short"
        );
    }

    #[test]
    fn auto_fallback_never_bypasses_focus_or_security_rejection() {
        let secure = VoxError::new(
            ErrorCategory::Permission,
            "fcitx.bridge_rejected",
            "secure context",
        );
        let missing_focus = VoxError::new(
            ErrorCategory::InvalidState,
            "fcitx.bridge_rejected",
            "no focused context",
        );
        let unavailable = VoxError::new(
            ErrorCategory::Unavailable,
            "fcitx.transport_failed",
            "socket unavailable",
        );
        assert!(!may_auto_fallback_from_fcitx(&secure));
        assert!(!may_auto_fallback_from_fcitx(&missing_focus));
        assert!(may_auto_fallback_from_fcitx(&unavailable));
        assert_eq!(
            select_auto_insertion(Err(unavailable)),
            Ok(ArmedInsertion::Copy)
        );
        assert_eq!(
            select_auto_insertion(Err(secure))
                .expect_err("secure context must be rejected")
                .code(),
            "fcitx.bridge_rejected"
        );
    }

    #[test]
    fn demo_only_profile_is_not_a_real_recognition_route() {
        let mut config: Config = toml::from_str(
            r#"schema_version = 1
default_profile = "demo"
[desktop]
[audio]
[profiles.demo]
primary = "demo"
[providers.demo]
kind = "mock"
text = "fixed text"
"#,
        )
        .expect("demo config");
        assert!(profile_is_demo_only(&config, &config.profiles["demo"]));

        config.providers.insert(
            "local".to_owned(),
            ProviderConfig::Command {
                program: "/usr/bin/true".to_owned(),
                args: Vec::new(),
                timeout_seconds: 30,
            },
        );
        config
            .profiles
            .get_mut("demo")
            .expect("demo profile")
            .fallbacks
            .push("local".to_owned());
        assert!(!profile_is_demo_only(&config, &config.profiles["demo"]));
    }

    #[test]
    fn command_provider_returns_stdout() {
        let args = vec!["-c".to_owned(), "printf '本地文本'".to_owned()];
        let text = transcribe_command("/bin/sh", &args, 1, Path::new("/tmp/audio.wav"), "zh")
            .expect("command provider output");
        assert_eq!(text, "本地文本");
    }

    #[test]
    fn command_provider_times_out() {
        let marker =
            std::env::temp_dir().join(format!("voxtype-provider-timeout-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let command = format!("(sleep 2; touch '{}') & wait", marker.to_string_lossy());
        let args = vec!["-c".to_owned(), command];
        let error = transcribe_command("/bin/sh", &args, 1, Path::new("/tmp/audio.wav"), "zh")
            .expect_err("command must time out");
        assert_eq!(error.code(), "provider.command_timeout");
        assert_eq!(error.category(), ErrorCategory::Timeout);
        assert!(error.is_retryable());
        std::thread::sleep(Duration::from_millis(1_500));
        assert!(!marker.exists(), "descendant process survived timeout");
    }

    #[test]
    fn command_provider_cancellation_kills_the_process_group() {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            trigger.cancel();
        });
        let started = Instant::now();
        let error = transcribe_command_cancellable(
            "/bin/sleep",
            &["5".to_owned()],
            10,
            Path::new("/tmp/audio.wav"),
            "zh",
            &cancellation,
        )
        .expect_err("command must be cancelled");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
        canceller.join().expect("canceller thread");
    }

    #[test]
    fn worker_rejects_pre_cancelled_work() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = run_recognition_worker(
            vec![(
                "mock".to_owned(),
                ProviderConfig::Mock {
                    text: "late".to_owned(),
                },
            )],
            ReplayPolicy::Never,
            Path::new("/tmp/audio.pcm"),
            "zh",
            &cancellation,
        );
        assert!(result.attempts.is_empty());
        assert_eq!(
            result.outcome.expect_err("cancelled outcome").category(),
            ErrorCategory::Cancelled
        );
    }

    #[test]
    fn worker_does_not_replay_audio_without_consent() {
        let result = run_recognition_worker(
            vec![
                (
                    "failure".to_owned(),
                    ProviderConfig::Command {
                        program: "/bin/false".to_owned(),
                        args: Vec::new(),
                        timeout_seconds: 1,
                    },
                ),
                (
                    "mock".to_owned(),
                    ProviderConfig::Mock {
                        text: "must not run".to_owned(),
                    },
                ),
            ],
            ReplayPolicy::Never,
            Path::new("/tmp/audio.pcm"),
            "zh",
            &CancellationToken::new(),
        );
        assert_eq!(result.attempts.len(), 1);
        assert!(result.attempts[0].transport_started);
        assert_eq!(
            result.attempts[0].audio_acceptance,
            AudioAcceptance::Accepted
        );
        assert!(result.outcome.is_err());
    }

    #[test]
    fn worker_can_fallback_when_transport_never_started() {
        let result = run_recognition_worker(
            vec![
                (
                    "missing-command".to_owned(),
                    ProviderConfig::Command {
                        program: "/definitely/missing/voxtype-provider".to_owned(),
                        args: Vec::new(),
                        timeout_seconds: 1,
                    },
                ),
                (
                    "demo".to_owned(),
                    ProviderConfig::Mock {
                        text: "fallback result".to_owned(),
                    },
                ),
            ],
            ReplayPolicy::BeforeAudioAccepted,
            Path::new("/tmp/audio.pcm"),
            "zh",
            &CancellationToken::new(),
        );
        assert_eq!(result.attempts.len(), 2);
        assert!(!result.attempts[0].transport_started);
        assert_eq!(
            result.attempts[0].audio_acceptance,
            AudioAcceptance::NotAccepted
        );
        assert_eq!(
            result.outcome.expect("fallback succeeds").transcript.text,
            "fallback result"
        );
    }

    #[test]
    fn command_provider_output_is_bounded() {
        let error = transcribe_command(
            "/usr/bin/head",
            &[
                "-c".to_owned(),
                "1048577".to_owned(),
                "/dev/zero".to_owned(),
            ],
            2,
            Path::new("/tmp/audio.wav"),
            "zh",
        )
        .expect_err("oversized output must fail");
        assert_eq!(error.code(), "provider.command_output_too_large");
    }
}
