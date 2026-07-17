//! Blocking D-Bus client used by the thin command-line interface.

use crate::{DBUS_INTERFACE, DBUS_NAME, DBUS_PATH};
use zbus::blocking::{Connection, Proxy};

pub struct Client<'a> {
    proxy: Proxy<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionResult {
    pub session: String,
    pub outcome: String,
    pub error_code: String,
    pub backend: String,
    pub char_count: u64,
}

impl<'a> Client<'a> {
    /// Connects to the per-user `VoxType` daemon.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the session bus or daemon is unavailable.
    pub fn connect(connection: &'a Connection) -> zbus::Result<Self> {
        let proxy = Proxy::new(connection, DBUS_NAME, DBUS_PATH, DBUS_INTERFACE)?;
        Ok(Self { proxy })
    }

    /// Returns the current state name.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if the daemon does not answer.
    pub fn status(&self) -> zbus::Result<String> {
        self.proxy.call("Status", &())
    }

    /// Returns provider availability and consecutive failure counts.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon does not answer.
    pub fn provider_status(&self) -> zbus::Result<String> {
        self.proxy.call("ProviderStatus", &())
    }

    /// Returns JSON containing session-local consumption and configured quotas.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon does not answer.
    pub fn usage_status(&self) -> zbus::Result<String> {
        self.proxy.call("UsageStatus", &())
    }

    /// Returns the most recently inserted transcript held in daemon memory.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon does not answer.
    pub fn last_transcript(&self) -> zbus::Result<String> {
        self.proxy.call("LastTranscript", &())
    }

    /// Returns up to twenty recent transcripts held in daemon memory.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon does not answer.
    pub fn transcript_history(&self) -> zbus::Result<Vec<String>> {
        self.proxy.call("TranscriptHistory", &())
    }

    /// Checks local typography and cleanup rules for the most recent transcript.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when no transcript is available.
    pub fn check_last_grammar(&self) -> zbus::Result<String> {
        self.proxy.call("CheckLastGrammar", &())
    }

    /// Checks local cleanup rules against the focused Fcitx surrounding text.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error for missing focus, sensitive fields, unsupported
    /// surrounding text, or an empty review window.
    pub fn check_context_grammar(&self) -> zbus::Result<String> {
        self.proxy.call("CheckContextGrammar", &())
    }

    /// Clears the in-memory recent transcript.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon does not answer.
    pub fn clear_history(&self) -> zbus::Result<()> {
        self.proxy.call("ClearHistory", &())
    }

    /// Starts recording and returns the new session ID.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if the state transition or audio capture fails.
    pub fn start(&self, profile: &str) -> zbus::Result<String> {
        self.proxy.call("Start", &(profile))
    }

    /// Stops recording, starts background recognition, and returns acceptance metadata.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if there is no matching active session.
    pub fn stop(&self, session: &str) -> zbus::Result<String> {
        self.proxy.call("Stop", &(session))
    }

    /// Stops recording and waits for the matching final session outcome.
    ///
    /// The result never contains transcript text. It is suitable for CLI and
    /// automation callers that must distinguish acceptance from completion.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when stopping fails, the signal body is malformed,
    /// or the daemon disconnects before publishing the final outcome.
    pub fn stop_wait(&self, session: &str) -> zbus::Result<SessionResult> {
        let mut signals = self.proxy.receive_signal("SessionFinished")?;
        let accepted = self.stop(session)?;
        let requested = if session.is_empty() {
            accepted
                .split_whitespace()
                .find_map(|part| part.strip_prefix("session="))
                .unwrap_or_default()
                .to_owned()
        } else {
            session.to_owned()
        };
        for message in &mut signals {
            let (signal_session, outcome, error_code, backend, char_count): (
                String,
                String,
                String,
                String,
                u64,
            ) = message.body().deserialize()?;
            if signal_session == requested {
                return Ok(SessionResult {
                    session: signal_session,
                    outcome,
                    error_code,
                    backend,
                    char_count,
                });
            }
        }
        Err(zbus::Error::Failure(
            "daemon disconnected before SessionFinished".to_owned(),
        ))
    }

    /// Toggles recording.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if recording cannot start or stop.
    pub fn toggle(&self, profile: &str) -> zbus::Result<String> {
        self.proxy.call("Toggle", &(profile))
    }

    /// Cancels the active recording.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if the session does not exist or does not match.
    pub fn cancel(&self, session: &str) -> zbus::Result<()> {
        self.proxy.call("Cancel", &(session))
    }

    /// Resets a terminal session to idle.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when work is still active.
    pub fn reset(&self) -> zbus::Result<()> {
        self.proxy.call("Reset", &())
    }

    /// Inserts explicit diagnostic text into the focused application.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when clipboard or paste injection fails.
    pub fn insert_test(&self, text: &str) -> zbus::Result<String> {
        self.proxy.call("InsertTest", &(text))
    }

    /// Reloads and validates the daemon configuration.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if the configuration is unavailable or invalid.
    pub fn reload_configuration(&self) -> zbus::Result<()> {
        self.proxy.call("ReloadConfiguration", &())
    }
}
