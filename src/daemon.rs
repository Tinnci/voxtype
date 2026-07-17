//! D-Bus daemon interface.

use crate::{
    audio::{Recording, RecordingResult, cleanup_stale_recordings},
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
use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::{
    Mutex,
    mpsc::{Receiver, TryRecvError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant};
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
    quit: bool,
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

    fn record_retryable_failure_at(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= 3 {
            self.blocked_until = Some(now + Duration::from_secs(60));
        }
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

    /// Returns a compact snapshot of configured provider health.
    fn provider_status(&self) -> String {
        let now = Instant::now();
        self.config
            .providers
            .keys()
            .map(|id| {
                let state = self.provider_health.get(id);
                let available = state.is_none_or(|health| health.is_available_at(now));
                let failures = state.map_or(0, |health| health.consecutive_failures);
                format!("{id}:available={available},failures={failures}")
            })
            .collect::<Vec<_>>()
            .join(" ")
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
        let body = if report.is_clean() {
            "No local grammar issues found".to_owned()
        } else {
            report
                .issues
                .iter()
                .take(2)
                .map(|issue| issue.message)
                .collect::<Vec<_>>()
                .join(" · ")
        };
        overlay("grammar", "Local text cleanup", &body, 5_000);
        Ok(report.render())
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
            .machine
            .apply(Command::Start(request))
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
                let _effect = self.machine.apply(Command::Fail {
                    session,
                    error: error.clone(),
                });
                return Err(map_error(error));
            }
        }

        match Recording::start() {
            Ok(recording) => {
                self.recording = Some(recording);
                self.recording_started_at = Some(Instant::now());
                self.active_profile = Some(profile_name);
                self.machine
                    .apply(Command::CaptureReady {
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
                let _effect = self.machine.apply(Command::Fail {
                    session,
                    error: domain_error,
                });
                Err(fdo::Error::Failed(format!("audio capture failed: {error}")))
            }
        }
    }

    fn stop(&mut self, session: &str) -> fdo::Result<String> {
        let active = self.active_session_id(session)?;
        self.machine
            .apply(Command::Stop {
                session: active.clone(),
            })
            .map_err(map_error)?;
        let recording = self
            .recording
            .take()
            .ok_or_else(|| fdo::Error::Failed("recording process is missing".to_owned()))?;
        self.recording_started_at = None;
        let result = recording
            .stop()
            .map_err(|error| fdo::Error::Failed(format!("failed to stop capture: {error}")))?;
        overlay(
            "processing",
            "Processing speech",
            "Running VAD and recognition",
            0,
        );
        let response = self.begin_recognition(&active, &result);
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
        self.machine
            .apply(Command::Cancel {
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
        self.machine.apply(Command::Reset).map_err(map_error)?;
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
                usage.record_failure();
                self.provider_failed(&attempt.provider_id, error.is_retryable());
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

    fn cleanup_recognition_job(&self, job: &RecognitionJob) {
        if !self.config.desktop.retain_recordings {
            let _ = fs::remove_file(&job.recording_path);
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
        let _effect = self.machine.apply(Command::Fail {
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
            .machine
            .apply(Command::TranscriptReady {
                session: session.clone(),
                text: text.clone(),
            })
            .map_err(map_error)?;
        let CommandEffect::InsertText { .. } = effect else {
            return Err(fdo::Error::Failed(
                "state machine did not request insertion".to_owned(),
            ));
        };
        let insertion = self.insert_text(session, &text).map_err(map_error)?;
        self.machine
            .apply(Command::InsertionComplete {
                session: session.clone(),
            })
            .map_err(map_error)?;
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
            let detail = if self.config.desktop.transcript_history_enabled {
                "Sent to the focused input path · Meta+Alt+G checks recent text"
            } else {
                "Sent to the focused input path · transcript history is off"
            };
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
        self.machine
            .apply(Command::NoSpeech {
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
        self.machine
            .apply(Command::NoSpeech {
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
        self.provider_health.remove(provider_id);
    }

    fn provider_failed(&mut self, provider_id: &str, retryable: bool) {
        if !retryable {
            return;
        }
        let health = self
            .provider_health
            .entry(provider_id.to_owned())
            .or_default();
        health.record_retryable_failure_at(Instant::now());
    }
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
    let _ = ProcessCommand::new("voxtype-overlay")
        .args(["show", state, title, body, &timeout_millis.to_string()])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_health_blocks_after_three_retryable_failures() {
        let now = Instant::now();
        let mut health = ProviderHealthState::default();

        health.record_retryable_failure_at(now);
        health.record_retryable_failure_at(now);
        assert!(health.is_available_at(now));

        health.record_retryable_failure_at(now);
        assert!(!health.is_available_at(now + Duration::from_secs(59)));
        assert!(health.is_available_at(now + Duration::from_secs(60)));
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
