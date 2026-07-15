//! D-Bus daemon interface.

use crate::{
    audio::{Recording, RecordingResult},
    config::{Config, InsertionBackend, ProviderConfig, lookup_secret},
    desktop::ClipboardInserter,
    fcitx::FcitxBridge,
};
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};
use voxtype_core::{
    Command, CommandEffect, ErrorCategory, ReplayPolicy, SessionId, SessionMachine, StartRequest,
    TriggerMode, VoxError,
};
use voxtype_provider_rest::{RestProviderConfig, transcribe_pcm};
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
    quit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArmedInsertion {
    Fcitx,
    Clipboard,
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

    fn start(&mut self, profile: &str) -> fdo::Result<String> {
        if self.recording.is_some() {
            return Err(fdo::Error::Failed("recording is already active".to_owned()));
        }
        let (profile_name, _profile) = self
            .config
            .profile((!profile.is_empty()).then_some(profile))
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown profile: {profile}")))?;
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
            InsertionBackend::Auto => Ok(if FcitxBridge.arm(&session).is_ok() {
                ArmedInsertion::Fcitx
            } else {
                ArmedInsertion::Clipboard
            }),
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
                self.active_profile = Some(profile_name);
                self.machine
                    .apply(Command::CaptureReady {
                        session: session.clone(),
                    })
                    .map_err(map_error)?;
                notify("VoxType", "Listening…");
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
        let result = recording
            .stop()
            .map_err(|error| fdo::Error::Failed(format!("failed to stop capture: {error}")))?;
        let response = self.finish_recognition(&active, &result);
        if response.is_err() && self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(&active);
        }
        if !self.config.desktop.retain_recordings {
            let _result = fs::remove_file(&result.path);
        }
        self.active_profile = None;
        self.armed_insertion = None;
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
        if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
            FcitxBridge.cancel(&active);
        }
        self.active_profile = None;
        self.armed_insertion = None;
        notify("VoxType", "Dictation cancelled");
        Ok(())
    }

    fn reset(&mut self) -> fdo::Result<()> {
        self.machine.apply(Command::Reset).map_err(map_error)?;
        Ok(())
    }

    fn reload_configuration(&mut self) -> fdo::Result<()> {
        let config = Config::load_or_create().map_err(map_error)?;
        self.inserter = ClipboardInserter::default().with_restore(config.desktop.restore_clipboard);
        self.config = config;
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
        if self.armed_insertion == Some(ArmedInsertion::Fcitx)
            && let Some(session) = self.machine.state().session()
        {
            FcitxBridge.cancel(session);
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
        let inserter = ClipboardInserter::default().with_restore(config.desktop.restore_clipboard);
        Ok(Self {
            machine: SessionMachine::default(),
            recording: None,
            inserter,
            config,
            active_profile: None,
            armed_insertion: None,
            provider_health: std::collections::BTreeMap::new(),
            quit: false,
        })
    }

    #[must_use]
    pub const fn should_quit_value(&self) -> bool {
        self.quit
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

    fn finish_recognition(
        &mut self,
        session: &SessionId,
        recording: &RecordingResult,
    ) -> fdo::Result<String> {
        if recording.duration_millis < self.config.audio.minimum_duration_millis {
            if self.armed_insertion == Some(ArmedInsertion::Fcitx) {
                FcitxBridge.cancel(session);
            }
            self.machine
                .apply(Command::NoSpeech {
                    session: session.clone(),
                })
                .map_err(map_error)?;
            notify("VoxType", "Recording was too short");
            return Ok(format!(
                "session={session} result=no-speech duration_ms={}",
                recording.duration_millis
            ));
        }

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

        let mut last_error = None;
        let mut attempted_provider = false;
        for provider_id in &providers {
            if !self.provider_is_available(provider_id) {
                continue;
            }
            if attempted_provider && replay != ReplayPolicy::BufferedWithConsent {
                break;
            }
            attempted_provider = true;
            match self.transcribe_with(provider_id, &recording.path, &language) {
                Ok(text) => {
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
                    notify("VoxType", "Dictation inserted");
                    return Ok(format!(
                        "session={session} provider={provider_id} chars={} backend={} clipboard_restored={}",
                        text.chars().count(),
                        insertion.backend,
                        insertion.clipboard_restored
                    ));
                }
                Err(error) => {
                    let retryable = error.is_retryable();
                    self.provider_failed(provider_id, retryable);
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                }
            }
        }

        let error = last_error.unwrap_or_else(|| {
            VoxError::new(
                ErrorCategory::Unavailable,
                "provider.no_route",
                "no provider was attempted",
            )
        });
        let message = error.to_string();
        let _effect = self.machine.apply(Command::Fail {
            session: session.clone(),
            error,
        });
        notify("VoxType recognition failed", &message);
        Err(fdo::Error::Failed(message))
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

    fn transcribe_with(
        &self,
        provider_id: &str,
        pcm_path: &Path,
        language: &str,
    ) -> Result<String, VoxError> {
        let provider = self.config.providers.get(provider_id).ok_or_else(|| {
            VoxError::new(
                ErrorCategory::Configuration,
                "provider.not_found",
                format!("provider {provider_id} is not configured"),
            )
        })?;
        match provider {
            ProviderConfig::Mock { text } => {
                if text.trim().is_empty() {
                    Err(VoxError::new(
                        ErrorCategory::Protocol,
                        "provider.mock_empty",
                        "mock provider text is empty",
                    ))
                } else {
                    Ok(text.clone())
                }
            }
            ProviderConfig::OpenaiCompatible {
                endpoint,
                model,
                secret,
                timeout_seconds,
            } => {
                let api_key = lookup_secret(secret)?;
                transcribe_pcm(
                    &RestProviderConfig {
                        endpoint: endpoint.clone(),
                        model: model.clone(),
                        api_key,
                        timeout_seconds: *timeout_seconds,
                    },
                    pcm_path,
                    language,
                )
                .map(|result| result.text)
            }
        }
    }

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
}
