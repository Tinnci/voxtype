//! D-Bus daemon interface.

use crate::{
    audio::{ProcessCaptureAdapter, cleanup_stale_recordings},
    config::{Config, InsertionBackend, ProfileConfig, ProviderConfig},
    fcitx::FcitxBridge,
    grammar,
    insertion::DesktopInsertionAdapter,
    provider_adapters::build_provider_registry,
    vad::{self, VadConfig, VadResult},
};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Command as ProcessCommand;
use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, TryRecvError, sync_channel},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use voxtype_app::{
    AppController, CaptureAdapter, CaptureFrameMetrics, CaptureSession, CapturedAudio,
    InsertionAdapter, InsertionArm, InsertionMode, InsertionOutcome, ProviderHealthSnapshot,
    ProviderTranscript, RecognitionInput, RecognitionRouteResult, WakeHandle,
    run_recognition_route,
};
use voxtype_core::{
    CancellationToken, Command, CommandEffect, ErrorCategory, ProviderId, ReplayPolicy,
    RoutingPolicy, SessionId, StartRequest, TriggerMode, VoxError,
};
use zbus::fdo;

pub use voxtype_app::AppEvent as DaemonEvent;

#[derive(Debug)]
pub struct VoxTypeDaemon {
    app: AppController,
    capture_adapter: Arc<dyn CaptureAdapter>,
    recording: Option<Box<dyn CaptureSession>>,
    insertion_adapter: Arc<dyn InsertionAdapter>,
    config: Config,
    active_profile: Option<String>,
    armed_insertion: Option<InsertionArm>,
    transcript_history: VecDeque<String>,
    recording_started_at: Option<Instant>,
    recognition_job: Option<RecognitionJob>,
    live_vad: Option<vad::StreamingVad>,
    last_live_audio: Option<vad::VadFrameAnalysis>,
    last_audio_overlay_at: Option<Instant>,
    desktop_feedback_enabled: bool,
    quit: bool,
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
    receiver: Mutex<Receiver<RecognitionRouteResult>>,
    recording_path: std::path::PathBuf,
    vad_result: Option<VadResult>,
    audio_millis: u64,
}

#[zbus::interface(name = "io.github.tinnci.VoxType1")]
impl VoxTypeDaemon {
    fn status(&self) -> String {
        self.app.state().name().to_owned()
    }

    fn active_session(&self) -> String {
        self.app
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
        provider_status_json(&self.config, &self.app, Instant::now())
    }

    /// Returns session-local consumption counters and configured soft limits.
    fn usage_status(&self) -> String {
        let providers = self
            .config
            .providers
            .keys()
            .map(|id| {
                let usage = ProviderId::new(id.clone())
                    .map(|provider_id| self.app.provider_usage_snapshot(&provider_id))
                    .unwrap_or_default();
                let quota = self.config.quotas.get(id).cloned().unwrap_or_default();
                (
                    id.clone(),
                    serde_json::json!({
                        "usage": {
                            "attempts": usage.attempts,
                            "requests": usage.requests,
                            "successes": usage.successes,
                            "failures": usage.failures,
                            "audio_millis": usage.audio_millis,
                            "token_reports": usage.token_reports,
                            "input_tokens": usage.input_tokens,
                            "output_tokens": usage.output_tokens,
                            "total_tokens": usage.total_tokens,
                            "reported_tokens": usage.reported_tokens,
                        },
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

    /// Returns a retained transcript-free terminal result for one session.
    fn session_result(&self, session: &str) -> fdo::Result<(bool, String, String, String, u64)> {
        if session.is_empty() || session.len() > 128 || session.chars().any(char::is_control) {
            return Err(fdo::Error::InvalidArgs(
                "session ID must be non-empty and bounded".to_owned(),
            ));
        }
        Ok(self.app.session_result(session).map_or_else(
            || (false, String::new(), String::new(), String::new(), 0),
            |result| {
                (
                    true,
                    result.outcome.clone(),
                    result.error_code.clone(),
                    result.backend.clone(),
                    result.char_count,
                )
            },
        ))
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
        self.present_overlay("grammar", "Local text cleanup", &body, 5_000);
        Ok(report.render())
    }

    #[allow(clippy::unused_self)] // Public D-Bus action intentionally has no daemon-held text state.
    fn check_context_grammar(&self) -> fdo::Result<String> {
        let context = match FcitxBridge.context() {
            Ok(context) => context,
            Err(error) => {
                self.present_overlay("error", "Focused text unavailable", error.message(), 3_000);
                return Err(map_error(error));
            }
        };
        let text = context.review_text();
        if text.trim().is_empty() {
            self.present_overlay(
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
        self.present_overlay("grammar", "Focused text cleanup", &body, 5_000);
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

        let insertion_mode = match self.config.desktop.insertion_backend {
            InsertionBackend::Fcitx => InsertionMode::Fcitx,
            InsertionBackend::Clipboard => InsertionMode::Clipboard,
            InsertionBackend::Copy => InsertionMode::Copy,
            InsertionBackend::Auto => InsertionMode::Auto,
        };
        match self.insertion_adapter.arm(insertion_mode, &session) {
            Ok(armed) => self.armed_insertion = Some(armed),
            Err(error) => {
                let _effect = self.apply_command(Command::Fail {
                    session,
                    error: error.clone(),
                });
                return Err(map_error(error));
            }
        }

        match self
            .capture_adapter
            .start(Some(self.config.audio.device.as_str()))
        {
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
                self.present_notification("VoxType", "Listening…");
                self.present_overlay(
                    "listening",
                    "Listening",
                    "Speak now · shortcut again to stop",
                    0,
                );
                Ok(session.to_string())
            }
            Err(error) => {
                if let Some(armed) = self.armed_insertion.as_ref() {
                    self.insertion_adapter.cancel(armed);
                }
                self.armed_insertion = None;
                let _effect = self.apply_command(Command::Fail {
                    session,
                    error: error.clone(),
                });
                Err(map_error(error))
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
                let _effect = self.apply_command(Command::Fail {
                    session: active,
                    error: error.clone(),
                });
                return Err(map_error(error));
            }
        };
        self.present_overlay(
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
        if response.is_err() {
            if let Some(armed) = self.armed_insertion.as_ref() {
                self.insertion_adapter.cancel(armed);
            }
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
        if let Some(armed) = self.armed_insertion.as_ref() {
            self.insertion_adapter.cancel(armed);
        }
        self.active_profile = None;
        self.armed_insertion = None;
        self.present_notification("VoxType", "Dictation cancelled");
        self.present_overlay(
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
        let registry = build_provider_registry(&config).map_err(map_error)?;
        self.insertion_adapter = Arc::new(DesktopInsertionAdapter::new(
            config.desktop.restore_clipboard,
        ));
        self.config = config;
        self.app.replace_registry(registry);
        if !self.config.desktop.transcript_history_enabled {
            self.transcript_history.clear();
        }
        Ok(())
    }

    fn insert_test(&self, text: &str) -> fdo::Result<String> {
        let result = self
            .insertion_adapter
            .insert_diagnostic(text)
            .map_err(map_error)?;
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
        if let Some(armed) = self.armed_insertion.as_ref() {
            self.insertion_adapter.cancel(armed);
        }
        self.quit = true;
        self.app.wake_handle().notify();
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
        Self::load_with_wake(WakeHandle::disabled())
    }

    /// Loads the service with a coalescing daemon-loop wake handle.
    ///
    /// # Errors
    ///
    /// Returns a normalized configuration error if startup composition fails.
    pub fn load_with_wake(wake: WakeHandle) -> Result<Self, VoxError> {
        let config = Config::load_or_create()?;
        if !config.desktop.retain_recordings {
            cleanup_stale_recordings();
        }
        let restore_clipboard = config.desktop.restore_clipboard;
        Self::compose_with_feedback(
            config,
            wake,
            Arc::new(ProcessCaptureAdapter),
            Arc::new(DesktopInsertionAdapter::new(restore_clipboard)),
            true,
        )
    }

    /// Composes the daemon from validated configuration and boundary adapters.
    ///
    /// This is the deterministic integration-test seam for fake capture and
    /// insertion implementations. It suppresses real desktop notification and
    /// overlay processes; production uses [`Self::load_with_wake`].
    ///
    /// # Errors
    ///
    /// Returns a configuration error if provider registry composition fails.
    pub fn compose(
        config: Config,
        wake: WakeHandle,
        capture_adapter: Arc<dyn CaptureAdapter>,
        insertion_adapter: Arc<dyn InsertionAdapter>,
    ) -> Result<Self, VoxError> {
        Self::compose_with_feedback(config, wake, capture_adapter, insertion_adapter, false)
    }

    fn compose_with_feedback(
        config: Config,
        wake: WakeHandle,
        capture_adapter: Arc<dyn CaptureAdapter>,
        insertion_adapter: Arc<dyn InsertionAdapter>,
        desktop_feedback_enabled: bool,
    ) -> Result<Self, VoxError> {
        config.validate()?;
        let registry = build_provider_registry(&config)?;
        Ok(Self {
            app: AppController::with_wake(registry, wake),
            capture_adapter,
            recording: None,
            insertion_adapter,
            config,
            active_profile: None,
            armed_insertion: None,
            transcript_history: VecDeque::with_capacity(20),
            recording_started_at: None,
            recognition_job: None,
            live_vad: None,
            last_live_audio: None,
            last_audio_overlay_at: None,
            desktop_feedback_enabled,
            quit: false,
        })
    }

    #[must_use]
    pub const fn should_quit_value(&self) -> bool {
        self.quit
    }

    /// Returns the longest time the daemon loop may wait without an event.
    ///
    /// Recognition completion and D-Bus state changes wake the loop directly.
    /// A short timer is retained only while recording for level telemetry and
    /// the maximum-duration safety deadline.
    #[must_use]
    pub fn maintenance_wait(&self) -> Duration {
        if self.quit {
            return Duration::ZERO;
        }
        let Some(started_at) = self.recording_started_at else {
            return Duration::from_secs(60);
        };
        let maximum = Duration::from_secs(self.config.audio.maximum_duration_seconds);
        Duration::from_millis(100).min(maximum.saturating_sub(started_at.elapsed()))
    }

    #[must_use]
    pub fn state_snapshot(&self) -> (String, String) {
        self.app.state_snapshot()
    }

    #[must_use]
    pub fn drain_events(&mut self) -> Vec<DaemonEvent> {
        self.app.drain_events()
    }

    fn apply_command(&mut self, command: Command) -> Result<CommandEffect, VoxError> {
        self.app.apply(command)
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
        self.present_overlay(
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
                Err(TryRecvError::Disconnected) => RecognitionRouteResult {
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
        let is_current = self.app.state().name() == "finalizing"
            && self.app.state().session() == Some(&job.session);
        if !is_current {
            self.cleanup_recognition_job(&job);
            return Ok(true);
        }

        self.app
            .record_route_result(&received, job.audio_millis, Instant::now());
        let result = match received.outcome {
            Ok(success) => self.complete_recognition(
                &job.session,
                success.provider_id.as_str(),
                success.transcript,
                job.vad_result,
            ),
            Err(error) => self.fail_recognition(&job.session, error),
        };
        if result.is_err() {
            if let Some(armed) = self.armed_insertion.as_ref() {
                self.insertion_adapter.cancel(armed);
            }
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
            .map(|recording| recording.drain_metrics())
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
        for CaptureFrameMetrics {
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
        self.present_audio_metrics(analysis, clip_percent);
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
        let is_active = self.app.state().session() == Some(session)
            && matches!(
                self.app.state().name(),
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
            .app
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
        recording: &CapturedAudio,
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
        let primary = ProviderId::new(profile.primary.clone()).map_err(map_error)?;
        let fallbacks = profile
            .fallbacks
            .iter()
            .cloned()
            .map(ProviderId::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_error)?;
        let policy = RoutingPolicy {
            primary,
            fallbacks,
            replay: ReplayPolicy::from(profile.replay),
        };
        let language = profile.language.clone();
        let route = match self.app.plan_route(&policy, Instant::now()) {
            Ok(route) => route,
            Err(error) => return self.fail_recognition(session, error),
        };
        let registry = self.app.registry();
        let wake = self.app.wake_handle();

        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let path = recording.path.clone();
        let worker_path = path.clone();
        let (sender, receiver) = sync_channel(1);
        thread::Builder::new()
            .name(format!("voxtype-recognition-{session}"))
            .spawn(move || {
                let result = run_recognition_route(
                    &registry,
                    &route,
                    RecognitionInput {
                        pcm_path: &worker_path,
                        language: &language,
                    },
                    &worker_cancellation,
                );
                let _result = sender.send(result);
                wake.notify();
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
        self.present_notification("VoxType recognition failed", &message);
        self.present_overlay(
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
        let ProviderTranscript { text, usage: _ } = transcript;
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
        self.app
            .finish_session(
                session,
                "completed",
                "",
                insertion.backend,
                u64::try_from(text.chars().count()).unwrap_or(u64::MAX),
            )
            .map_err(map_error)?;
        if self.config.desktop.transcript_history_enabled {
            if self.transcript_history.len() == 20 {
                self.transcript_history.pop_front();
            }
            self.transcript_history.push_back(text.clone());
        }
        if insertion.backend == "copy-only" {
            self.present_notification("VoxType", "Dictation copied to clipboard");
            self.present_overlay(
                "done",
                "Text copied",
                "Paste it when the intended field is ready",
                3_000,
            );
        } else {
            self.present_notification("VoxType", "Dictation dispatched");
            let detail = "Sent to the focused input path · Meta+Alt+G reviews focused text";
            self.present_overlay("done", "Text dispatched", detail, 2_000);
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
        recording: &CapturedAudio,
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
        if let Some(armed) = self.armed_insertion.as_ref() {
            self.insertion_adapter.cancel(armed);
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
        self.present_notification("VoxType", "No speech detected");
        self.present_overlay("no-speech", "No speech detected", guidance, 2_800);
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
        recording: &CapturedAudio,
    ) -> fdo::Result<Option<String>> {
        if recording.duration_millis >= self.config.audio.minimum_duration_millis {
            return Ok(None);
        }
        if let Some(armed) = self.armed_insertion.as_ref() {
            self.insertion_adapter.cancel(armed);
        }
        self.apply_command(Command::NoSpeech {
            session: session.clone(),
        })
        .map_err(map_error)?;
        self.present_notification("VoxType", "Recording was too short");
        self.present_overlay(
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

fn provider_status_json(config: &Config, app: &AppController, now: Instant) -> String {
    let providers = config
        .providers
        .keys()
        .map(|id| {
            let snapshot = ProviderId::new(id.clone()).map_or_else(
                |_| ProviderHealthSnapshot::default(),
                |provider_id| app.provider_health_snapshot(&provider_id, now),
            );
            let health = serde_json::json!({
                "route_available": snapshot.route_available,
                "verified": snapshot.verified,
                "verified_age_seconds": snapshot.verified_age_seconds,
                "verification_ttl_seconds": snapshot.verification_ttl_seconds,
                "consecutive_failures": snapshot.consecutive_failures,
                "retry_after_seconds": snapshot.retry_after_seconds,
                "last_failure_age_seconds": snapshot.last_failure_age_seconds,
                "last_error_category": snapshot
                    .last_error_category
                    .map(error_category_name),
                "last_error_code": snapshot.last_error_code,
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
    fn insert_text(&self, session: &SessionId, text: &str) -> Result<InsertionOutcome, VoxError> {
        let armed = self.armed_insertion.as_ref().ok_or_else(|| {
            VoxError::new(
                ErrorCategory::InvalidState,
                "desktop.not_armed",
                "no text insertion target was armed for the session",
            )
        })?;
        if &armed.session != session {
            return Err(VoxError::new(
                ErrorCategory::InvalidState,
                "desktop.session_mismatch",
                "text insertion target belongs to another session",
            ));
        }
        self.insertion_adapter.commit(armed, text)
    }

    fn present_notification(&self, summary: &str, body: &str) {
        if self.desktop_feedback_enabled {
            notify(summary, body);
        }
    }

    fn present_overlay(&self, state: &str, title: &str, body: &str, timeout_millis: u32) {
        if self.desktop_feedback_enabled {
            overlay(state, title, body, timeout_millis);
        }
    }

    fn present_audio_metrics(&self, analysis: vad::VadFrameAnalysis, clipping_percent: u32) {
        if self.desktop_feedback_enabled {
            overlay_audio_metrics(analysis, clipping_percent);
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FAKE_CAPTURE: AtomicU64 = AtomicU64::new(1);

    #[derive(Debug)]
    struct FakeCaptureAdapter;

    #[derive(Debug)]
    struct FakeCaptureSession {
        path: PathBuf,
    }

    impl CaptureAdapter for FakeCaptureAdapter {
        fn start(&self, _device: Option<&str>) -> Result<Box<dyn CaptureSession>, VoxError> {
            let sequence = NEXT_FAKE_CAPTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voxtype-fake-capture-{}-{sequence}.pcm",
                std::process::id()
            ));
            std::fs::write(&path, vec![0_u8; 32_000]).map_err(|error| {
                VoxError::new(
                    ErrorCategory::Internal,
                    "test.capture_write",
                    error.to_string(),
                )
            })?;
            Ok(Box::new(FakeCaptureSession { path }))
        }
    }

    impl CaptureSession for FakeCaptureSession {
        fn stop(self: Box<Self>) -> Result<CapturedAudio, VoxError> {
            Ok(CapturedAudio {
                path: self.path.clone(),
                bytes: 32_000,
                duration_millis: 1_000,
                backend: "fake-capture",
            })
        }

        fn cancel(self: Box<Self>) {
            let _result = std::fs::remove_file(&self.path);
        }

        fn drain_metrics(&mut self) -> Vec<CaptureFrameMetrics> {
            Vec::new()
        }
    }

    #[derive(Debug, Default)]
    struct FakeInsertionAdapter {
        committed: Mutex<Vec<String>>,
    }

    impl FakeInsertionAdapter {
        fn committed(&self) -> Vec<String> {
            self.committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl InsertionAdapter for FakeInsertionAdapter {
        fn arm(&self, _mode: InsertionMode, session: &SessionId) -> Result<InsertionArm, VoxError> {
            Ok(InsertionArm {
                session: session.clone(),
                backend: "fake-insertion",
            })
        }

        fn commit(&self, arm: &InsertionArm, text: &str) -> Result<InsertionOutcome, VoxError> {
            if arm.backend != "fake-insertion" {
                return Err(VoxError::new(
                    ErrorCategory::InvalidState,
                    "test.insertion_arm",
                    "unexpected fake insertion arm",
                ));
            }
            self.committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text.to_owned());
            Ok(InsertionOutcome {
                backend: "fake-insertion",
                clipboard_restored: true,
            })
        }

        fn cancel(&self, _arm: &InsertionArm) {}

        fn insert_diagnostic(&self, text: &str) -> Result<InsertionOutcome, VoxError> {
            self.committed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text.to_owned());
            Ok(InsertionOutcome {
                backend: "fake-insertion",
                clipboard_restored: true,
            })
        }
    }

    fn integration_config() -> Config {
        toml::from_str(
            r#"schema_version = 1
default_profile = "test"
[desktop]
[audio]
vad_enabled = false
minimum_duration_millis = 50
[profiles.test]
primary = "mock"
[providers.mock]
kind = "mock"
text = "完整链路文本"
"#,
        )
        .expect("integration config")
    }

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
        let registry = build_provider_registry(&config).expect("provider registry");
        VoxTypeDaemon {
            app: AppController::new(registry),
            capture_adapter: Arc::new(ProcessCaptureAdapter),
            recording: None,
            insertion_adapter: Arc::new(DesktopInsertionAdapter::new(true)),
            config,
            active_profile: None,
            armed_insertion: None,
            transcript_history: VecDeque::new(),
            recording_started_at: None,
            recognition_job: None,
            live_vad: None,
            last_live_audio: None,
            last_audio_overlay_at: None,
            desktop_feedback_enabled: false,
            quit: false,
        }
    }

    #[test]
    fn fake_capture_provider_and_insertion_complete_end_to_end() {
        let (wake, wake_receiver) = voxtype_app::wake_channel();
        let insertion = Arc::new(FakeInsertionAdapter::default());
        let mut daemon = VoxTypeDaemon::compose(
            integration_config(),
            wake,
            Arc::new(FakeCaptureAdapter),
            insertion.clone(),
        )
        .expect("compose daemon");

        let session = daemon.start("test").expect("start");
        assert_eq!(daemon.status(), "listening");
        let accepted = daemon.stop(&session).expect("stop");
        assert!(accepted.contains("result=processing"));

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if daemon.poll_recognition().expect("poll recognition") {
                break;
            }
            assert!(Instant::now() < deadline, "recognition did not finish");
            let _result = wake_receiver.recv_timeout(Duration::from_millis(100));
        }

        assert_eq!(daemon.status(), "completed");
        assert_eq!(insertion.committed(), vec!["完整链路文本"]);
        let terminal = daemon
            .session_result(&session)
            .expect("terminal result query");
        assert!(terminal.0);
        assert_eq!(terminal.1, "completed");
        assert_eq!(terminal.3, "fake-insertion");
    }

    #[test]
    fn private_dbus_round_trip_reaches_terminal_insertion() {
        use std::os::unix::net::UnixStream;
        use zbus::blocking::connection::Builder;

        let guid = zbus::Guid::generate();
        let (server_stream, client_stream) = UnixStream::pair().expect("socket pair");
        let (wake, wake_receiver) = voxtype_app::wake_channel();
        let insertion = Arc::new(FakeInsertionAdapter::default());
        let daemon = VoxTypeDaemon::compose(
            integration_config(),
            wake,
            Arc::new(FakeCaptureAdapter),
            insertion.clone(),
        )
        .expect("compose daemon");

        let server = std::thread::spawn(move || {
            let connection = Builder::unix_stream(server_stream)
                .server(guid)
                .expect("server guid")
                .p2p()
                .serve_at(crate::DBUS_PATH, daemon)
                .expect("serve daemon")
                .build()
                .expect("build server connection");
            loop {
                let interface = connection
                    .object_server()
                    .interface::<_, VoxTypeDaemon>(crate::DBUS_PATH)
                    .expect("daemon interface");
                let mut daemon = interface.get_mut();
                daemon.poll_audio_telemetry();
                daemon
                    .enforce_recording_deadline()
                    .expect("recording deadline");
                let _completed = daemon.poll_recognition();
                let should_quit = daemon.should_quit_value();
                let wait = daemon.maintenance_wait().min(Duration::from_secs(1));
                drop(daemon);
                drop(interface);
                if should_quit {
                    break;
                }
                let _result = wake_receiver.recv_timeout(wait);
            }
        });

        let client = Builder::unix_stream(client_stream)
            .p2p()
            .build()
            .expect("build client connection");

        let start = client
            .call_method(
                None::<&str>,
                crate::DBUS_PATH,
                Some("io.github.tinnci.VoxType1"),
                "Start",
                &"test",
            )
            .expect("D-Bus Start");
        let session: String = start.body().deserialize().expect("start session");
        client
            .call_method(
                None::<&str>,
                crate::DBUS_PATH,
                Some("io.github.tinnci.VoxType1"),
                "Stop",
                &session.as_str(),
            )
            .expect("D-Bus Stop");

        let deadline = Instant::now() + Duration::from_secs(2);
        let terminal = loop {
            let reply = client
                .call_method(
                    None::<&str>,
                    crate::DBUS_PATH,
                    Some("io.github.tinnci.VoxType1"),
                    "SessionResult",
                    &session.as_str(),
                )
                .expect("D-Bus SessionResult");
            let result: (bool, String, String, String, u64) =
                reply.body().deserialize().expect("terminal tuple");
            if result.0 {
                break result;
            }
            assert!(Instant::now() < deadline, "terminal result timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(terminal.1, "completed");
        assert_eq!(terminal.3, "fake-insertion");

        client
            .call_method(
                None::<&str>,
                crate::DBUS_PATH,
                Some("io.github.tinnci.VoxType1"),
                "Quit",
                &(),
            )
            .expect("D-Bus Quit");
        server.join().expect("server thread");
        assert_eq!(insertion.committed(), vec!["完整链路文本"]);
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
        daemon
            .app
            .finish_session(&session, "completed", "", "fcitx", 5)
            .expect("finish");

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
    fn terminal_result_survives_signal_queue_drain() {
        let mut daemon = event_test_daemon();
        let session = SessionId::from_counter(42);
        daemon
            .app
            .finish_session(&session, "completed", "", "fcitx", 12)
            .expect("finish");
        let _events = daemon.drain_events();

        let result = daemon
            .session_result(session.as_str())
            .expect("session result query");
        assert_eq!(
            result,
            (
                true,
                "completed".to_owned(),
                String::new(),
                "fcitx".to_owned(),
                12
            )
        );
    }

    #[test]
    fn provider_status_never_calls_an_unverified_route_healthy() {
        let daemon = event_test_daemon();
        let now = Instant::now();
        let initial: serde_json::Value =
            serde_json::from_str(&provider_status_json(&daemon.config, &daemon.app, now))
                .expect("provider status json");
        assert_eq!(initial["schema"], 1);
        assert_eq!(initial["providers"]["mock"]["route_available"], true);
        assert_eq!(initial["providers"]["mock"]["verified"], false);
        assert_eq!(
            initial["providers"]["mock"]["verification_ttl_seconds"],
            900
        );
    }

    #[test]
    fn recording_deadline_is_inclusive() {
        assert!(!recording_deadline_reached(Duration::from_secs(119), 120));
        assert!(recording_deadline_reached(Duration::from_secs(120), 120));
    }

    #[test]
    fn idle_wait_is_event_driven_and_recording_keeps_short_maintenance() {
        let mut daemon = event_test_daemon();
        assert_eq!(daemon.maintenance_wait(), Duration::from_secs(60));

        daemon.recording_started_at = Some(Instant::now());
        assert!(daemon.maintenance_wait() <= Duration::from_millis(100));
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
}
