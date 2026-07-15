//! D-Bus daemon interface.

use crate::{audio::Recording, desktop::ClipboardInserter};
use std::process::Command as ProcessCommand;
use voxtype_core::{Command, CommandEffect, SessionId, SessionMachine, StartRequest, TriggerMode};
use zbus::fdo;

#[derive(Debug, Default)]
pub struct VoxTypeDaemon {
    machine: SessionMachine,
    recording: Option<Recording>,
    inserter: ClipboardInserter,
    quit: bool,
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
        let request = StartRequest {
            mode: TriggerMode::Toggle,
            profile: (!profile.is_empty()).then(|| profile.to_owned()),
        };
        let effect = self
            .machine
            .apply(Command::Start(request))
            .map_err(map_error)?;
        let CommandEffect::BeginCapture { session, .. } = effect else {
            return Err(fdo::Error::Failed("invalid start effect".to_owned()));
        };

        match Recording::start() {
            Ok(recording) => {
                self.recording = Some(recording);
                self.machine
                    .apply(Command::CaptureReady {
                        session: session.clone(),
                    })
                    .map_err(map_error)?;
                notify("VoxType", "Listening…");
                Ok(session.to_string())
            }
            Err(error) => {
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
        self.machine
            .apply(Command::NoSpeech {
                session: active.clone(),
            })
            .map_err(map_error)?;
        notify(
            "VoxType",
            &format!("Captured {} ms of audio", result.duration_millis),
        );
        Ok(format!(
            "session={} bytes={} duration_ms={} path={}",
            active,
            result.bytes,
            result.duration_millis,
            result.path.display()
        ))
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
            .apply(Command::Cancel { session: active })
            .map_err(map_error)?;
        if let Some(recording) = self.recording.take() {
            recording.cancel();
        }
        notify("VoxType", "Dictation cancelled");
        Ok(())
    }

    fn reset(&mut self) -> fdo::Result<()> {
        self.machine.apply(Command::Reset).map_err(map_error)?;
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
        self.quit = true;
    }

    #[zbus(property)]
    fn should_quit(&self) -> bool {
        self.quit
    }
}

impl VoxTypeDaemon {
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
