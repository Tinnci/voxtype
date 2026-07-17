//! Single-session state machine.

use crate::{ErrorCategory, VoxError};
use std::fmt::{self, Display, Formatter};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(String);

impl SessionId {
    #[must_use]
    pub fn from_counter(counter: u64) -> Self {
        Self(format!("session-{counter}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for SessionId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerMode {
    PushToTalk,
    Toggle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartRequest {
    pub mode: TriggerMode,
    pub profile: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Start(StartRequest),
    Stop { session: SessionId },
    Cancel { session: SessionId },
    CaptureReady { session: SessionId },
    CaptureStopped { session: SessionId },
    NoSpeech { session: SessionId },
    TranscriptReady { session: SessionId, text: String },
    InsertionComplete { session: SessionId },
    Fail { session: SessionId, error: VoxError },
    Reset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionState {
    Idle,
    Preparing {
        session: SessionId,
        mode: TriggerMode,
    },
    Listening {
        session: SessionId,
        mode: TriggerMode,
    },
    Finalizing {
        session: SessionId,
    },
    Inserting {
        session: SessionId,
        text: String,
    },
    Completed {
        session: SessionId,
    },
    Cancelled {
        session: SessionId,
    },
    Failed {
        session: SessionId,
        error: VoxError,
    },
}

impl SessionState {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Preparing { .. } => "preparing",
            Self::Listening { .. } => "listening",
            Self::Finalizing { .. } => "finalizing",
            Self::Inserting { .. } => "inserting",
            Self::Completed { .. } => "completed",
            Self::Cancelled { .. } => "cancelled",
            Self::Failed { .. } => "failed",
        }
    }

    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Idle => None,
            Self::Preparing { session, .. }
            | Self::Listening { session, .. }
            | Self::Finalizing { session }
            | Self::Inserting { session, .. }
            | Self::Completed { session }
            | Self::Cancelled { session }
            | Self::Failed { session, .. } => Some(session),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandEffect {
    None,
    BeginCapture {
        session: SessionId,
        request: StartRequest,
    },
    StopCapture {
        session: SessionId,
    },
    CancelWork {
        session: SessionId,
    },
    InsertText {
        session: SessionId,
        text: String,
    },
}

#[derive(Debug)]
pub struct SessionMachine {
    state: SessionState,
    next_id: u64,
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            next_id: 1,
        }
    }
}

impl SessionMachine {
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    /// Applies one serialized command and returns work for an outer adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is invalid for the current state or
    /// targets a different session.
    pub fn apply(&mut self, command: Command) -> Result<CommandEffect, VoxError> {
        match command {
            Command::Start(request) => self.start(request),
            Command::Stop { session } => self.stop(&session),
            Command::Cancel { session } => self.cancel(&session),
            Command::CaptureReady { session } => self.capture_ready(&session),
            Command::CaptureStopped { session } => self.capture_stopped(&session),
            Command::NoSpeech { session } => self.no_speech(&session),
            Command::TranscriptReady { session, text } => self.transcript_ready(&session, text),
            Command::InsertionComplete { session } => self.insertion_complete(&session),
            Command::Fail { session, error } => self.fail(&session, error),
            Command::Reset => self.reset(),
        }
    }

    fn start(&mut self, request: StartRequest) -> Result<CommandEffect, VoxError> {
        if matches!(
            self.state,
            SessionState::Completed { .. }
                | SessionState::Cancelled { .. }
                | SessionState::Failed { .. }
        ) {
            self.state = SessionState::Idle;
        }
        if self.state != SessionState::Idle {
            return Err(invalid_state("a session is already active"));
        }
        let session = SessionId::from_counter(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.state = SessionState::Preparing {
            session: session.clone(),
            mode: request.mode,
        };
        Ok(CommandEffect::BeginCapture { session, request })
    }

    fn stop(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let SessionState::Listening { session, .. } = &self.state else {
            return Err(invalid_state("stop requires a listening session"));
        };
        ensure_session(session, requested)?;
        let session = session.clone();
        self.state = SessionState::Finalizing {
            session: session.clone(),
        };
        Ok(CommandEffect::StopCapture { session })
    }

    fn cancel(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let Some(active) = self.state.session() else {
            return Err(invalid_state("there is no session to cancel"));
        };
        ensure_session(active, requested)?;
        let session = active.clone();
        self.state = SessionState::Cancelled {
            session: session.clone(),
        };
        Ok(CommandEffect::CancelWork { session })
    }

    fn capture_ready(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let SessionState::Preparing { session, mode } = &self.state else {
            return Err(invalid_state("capture-ready requires a preparing session"));
        };
        ensure_session(session, requested)?;
        self.state = SessionState::Listening {
            session: session.clone(),
            mode: *mode,
        };
        Ok(CommandEffect::None)
    }

    fn capture_stopped(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let SessionState::Listening { session, .. } = &self.state else {
            return Err(invalid_state(
                "capture-stopped requires a listening session",
            ));
        };
        ensure_session(session, requested)?;
        self.state = SessionState::Finalizing {
            session: session.clone(),
        };
        Ok(CommandEffect::None)
    }

    fn transcript_ready(
        &mut self,
        requested: &SessionId,
        text: String,
    ) -> Result<CommandEffect, VoxError> {
        let SessionState::Finalizing { session } = &self.state else {
            return Err(invalid_state(
                "a final transcript requires a finalizing session",
            ));
        };
        ensure_session(session, requested)?;
        if text.trim().is_empty() {
            return Err(VoxError::new(
                ErrorCategory::Protocol,
                "transcript.empty",
                "provider returned an empty final transcript",
            ));
        }
        let session = session.clone();
        self.state = SessionState::Inserting {
            session: session.clone(),
            text: text.clone(),
        };
        Ok(CommandEffect::InsertText { session, text })
    }

    fn no_speech(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let SessionState::Finalizing { session } = &self.state else {
            return Err(invalid_state("no-speech requires a finalizing session"));
        };
        ensure_session(session, requested)?;
        self.state = SessionState::Completed {
            session: session.clone(),
        };
        Ok(CommandEffect::None)
    }

    fn insertion_complete(&mut self, requested: &SessionId) -> Result<CommandEffect, VoxError> {
        let SessionState::Inserting { session, .. } = &self.state else {
            return Err(invalid_state(
                "insertion-complete requires an inserting session",
            ));
        };
        ensure_session(session, requested)?;
        self.state = SessionState::Completed {
            session: session.clone(),
        };
        Ok(CommandEffect::None)
    }

    fn fail(&mut self, requested: &SessionId, error: VoxError) -> Result<CommandEffect, VoxError> {
        let Some(active) = self.state.session() else {
            return Err(invalid_state("there is no active session to fail"));
        };
        ensure_session(active, requested)?;
        self.state = SessionState::Failed {
            session: active.clone(),
            error,
        };
        Ok(CommandEffect::CancelWork {
            session: requested.clone(),
        })
    }

    fn reset(&mut self) -> Result<CommandEffect, VoxError> {
        if matches!(
            self.state,
            SessionState::Preparing { .. }
                | SessionState::Listening { .. }
                | SessionState::Finalizing { .. }
                | SessionState::Inserting { .. }
        ) {
            return Err(invalid_state("cannot reset an active session"));
        }
        self.state = SessionState::Idle;
        Ok(CommandEffect::None)
    }
}

fn invalid_state(message: &'static str) -> VoxError {
    VoxError::new(
        ErrorCategory::InvalidState,
        "session.invalid_state",
        message,
    )
}

fn ensure_session(active: &SessionId, requested: &SessionId) -> Result<(), VoxError> {
    if active == requested {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::InvalidArgument,
            "session.id_mismatch",
            "command targets a different session",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(machine: &mut SessionMachine) -> SessionId {
        let effect = machine
            .apply(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect("start must be accepted");
        let CommandEffect::BeginCapture { session, .. } = effect else {
            panic!("unexpected start effect");
        };
        session
    }

    #[test]
    fn completes_happy_path() {
        let mut machine = SessionMachine::default();
        let session = start(&mut machine);
        machine
            .apply(Command::CaptureReady {
                session: session.clone(),
            })
            .expect("capture becomes ready");
        machine
            .apply(Command::Stop {
                session: session.clone(),
            })
            .expect("stop is accepted");
        let effect = machine
            .apply(Command::TranscriptReady {
                session: session.clone(),
                text: "你好".to_owned(),
            })
            .expect("final transcript is accepted");
        assert_eq!(
            effect,
            CommandEffect::InsertText {
                session: session.clone(),
                text: "你好".to_owned()
            }
        );
        machine
            .apply(Command::InsertionComplete {
                session: session.clone(),
            })
            .expect("insertion completes");
        assert_eq!(machine.state().name(), "completed");
        machine
            .apply(Command::Reset)
            .expect("terminal state resets");
        assert_eq!(machine.state(), &SessionState::Idle);
    }

    #[test]
    fn rejects_parallel_session() {
        let mut machine = SessionMachine::default();
        let _session = start(&mut machine);
        let error = machine
            .apply(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect_err("parallel start must fail");
        assert_eq!(error.category(), ErrorCategory::InvalidState);
    }

    #[test]
    fn cancel_is_terminal_and_never_inserts() {
        let mut machine = SessionMachine::default();
        let session = start(&mut machine);
        let effect = machine
            .apply(Command::Cancel {
                session: session.clone(),
            })
            .expect("cancel is accepted");
        assert_eq!(effect, CommandEffect::CancelWork { session });
        assert_eq!(machine.state().name(), "cancelled");
    }

    #[test]
    fn late_transcript_after_cancel_is_rejected() {
        let mut machine = SessionMachine::default();
        let session = start(&mut machine);
        machine
            .apply(Command::CaptureReady {
                session: session.clone(),
            })
            .expect("capture becomes ready");
        machine
            .apply(Command::Stop {
                session: session.clone(),
            })
            .expect("stop is accepted");
        machine
            .apply(Command::Cancel {
                session: session.clone(),
            })
            .expect("cancel is accepted while finalizing");

        let error = machine
            .apply(Command::TranscriptReady {
                session,
                text: "stale result".to_owned(),
            })
            .expect_err("cancelled work must not insert a late transcript");
        assert_eq!(error.category(), ErrorCategory::InvalidState);
        assert_eq!(machine.state().name(), "cancelled");
    }

    #[test]
    fn old_session_result_cannot_complete_a_new_session() {
        let mut machine = SessionMachine::default();
        let first = start(&mut machine);
        machine
            .apply(Command::Cancel {
                session: first.clone(),
            })
            .expect("first session is cancelled");

        let second = start(&mut machine);
        machine
            .apply(Command::CaptureReady {
                session: second.clone(),
            })
            .expect("second capture becomes ready");
        machine
            .apply(Command::Stop {
                session: second.clone(),
            })
            .expect("second stop is accepted");

        let error = machine
            .apply(Command::TranscriptReady {
                session: first,
                text: "stale result".to_owned(),
            })
            .expect_err("a previous session cannot complete the active one");
        assert_eq!(error.category(), ErrorCategory::InvalidArgument);
        assert_eq!(machine.state().session(), Some(&second));

        let effect = machine
            .apply(Command::TranscriptReady {
                session: second.clone(),
                text: "current result".to_owned(),
            })
            .expect("current session transcript is accepted");
        assert_eq!(
            effect,
            CommandEffect::InsertText {
                session: second,
                text: "current result".to_owned(),
            }
        );
    }

    #[test]
    fn terminal_session_allows_next_start() {
        let mut machine = SessionMachine::default();
        let first = start(&mut machine);
        machine
            .apply(Command::Cancel { session: first })
            .expect("cancel is accepted");
        let second = start(&mut machine);
        assert_eq!(second.as_str(), "session-2");
    }
}
