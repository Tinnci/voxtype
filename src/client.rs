//! Blocking D-Bus client used by the thin command-line interface.

use crate::{DBUS_INTERFACE, DBUS_NAME, DBUS_PATH};
use std::thread;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};

const SESSION_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SESSION_RESULT_WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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

    /// Queries a retained transcript-free terminal result.
    ///
    /// `None` means the session is still running, unknown, or older than the
    /// daemon's bounded result cache.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when the daemon cannot answer the query.
    pub fn session_result(&self, session: &str) -> zbus::Result<Option<SessionResult>> {
        let (found, outcome, error_code, backend, char_count): (bool, String, String, String, u64) =
            self.proxy.call("SessionResult", &(session))?;
        Ok(found.then(|| SessionResult {
            session: session.to_owned(),
            outcome,
            error_code,
            backend,
            char_count,
        }))
    }

    /// Stops recording and waits for the matching final session outcome.
    ///
    /// The result never contains transcript text. It is suitable for CLI and
    /// automation callers that must distinguish acceptance from completion.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error when stopping fails, the daemon disconnects, or no
    /// terminal result becomes queryable within five minutes.
    pub fn stop_wait(&self, session: &str) -> zbus::Result<SessionResult> {
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
        if requested.is_empty() {
            return Err(zbus::Error::Failure(
                "daemon did not return a session ID while accepting Stop".to_owned(),
            ));
        }
        let deadline = Instant::now() + SESSION_RESULT_WAIT_TIMEOUT;
        loop {
            if let Some(result) = self.session_result(&requested)? {
                return Ok(result);
            }
            if Instant::now() >= deadline {
                return Err(zbus::Error::Failure(format!(
                    "timed out waiting for terminal result for session {requested}"
                )));
            }
            thread::sleep(SESSION_RESULT_POLL_INTERVAL);
        }
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
