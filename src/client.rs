//! Blocking D-Bus client used by the thin command-line interface.

use crate::{DBUS_INTERFACE, DBUS_NAME, DBUS_PATH};
use zbus::blocking::{Connection, Proxy};

pub struct Client<'a> {
    proxy: Proxy<'a>,
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

    /// Starts recording and returns the new session ID.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if the state transition or audio capture fails.
    pub fn start(&self, profile: &str) -> zbus::Result<String> {
        self.proxy.call("Start", &(profile))
    }

    /// Stops recording and returns capture metadata.
    ///
    /// # Errors
    ///
    /// Returns a D-Bus error if there is no matching active session.
    pub fn stop(&self, session: &str) -> zbus::Result<String> {
        self.proxy.call("Stop", &(session))
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
