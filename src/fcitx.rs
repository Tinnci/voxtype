//! Client for the minimal Fcitx5 input-context bridge.

use std::fs;
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use voxtype_core::{ErrorCategory, SessionId, VoxError};

static CLIENT_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default)]
pub struct FcitxBridge;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FcitxTarget {
    pub program: String,
    pub frontend: String,
}

impl FcitxBridge {
    /// Verifies that the Fcitx addon is available.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error when the addon socket cannot be reached.
    pub fn ping(self) -> Result<(), VoxError> {
        let response = request(&[b"PING"])?;
        expect_ok(&response)
    }

    /// Locks the currently focused, non-secure Fcitx input context to a session.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no focused context, the context is secure,
    /// or the addon is unavailable.
    pub fn arm(self, session: &SessionId) -> Result<(), VoxError> {
        self.arm_target(session).map(|_| ())
    }

    /// Arms the focused context and returns its application/frontend identity.
    ///
    /// # Errors
    ///
    /// Returns the same focus, security, and transport errors as [`Self::arm`].
    pub fn arm_target(self, session: &SessionId) -> Result<FcitxTarget, VoxError> {
        let response = request(&[b"ARM", session.as_str().as_bytes()])?;
        parse_arm_response(&response)
    }

    /// Probes the focused context without committing text.
    ///
    /// # Errors
    ///
    /// Returns an error if no safe Fcitx context currently has focus.
    pub fn probe(self) -> Result<FcitxTarget, VoxError> {
        let session = SessionId::from_counter(u64::MAX);
        let target = self.arm_target(&session)?;
        self.cancel(&session);
        Ok(target)
    }

    /// Arms and queues a diagnostic commit for the currently focused context.
    ///
    /// # Errors
    ///
    /// Returns an error for insecure or missing focus and for commit rejection.
    pub fn commit_test(self, text: &str) -> Result<FcitxTarget, VoxError> {
        if text.trim().is_empty() {
            return Err(VoxError::new(
                ErrorCategory::Protocol,
                "fcitx.empty_test",
                "diagnostic text is empty",
            ));
        }
        let session = SessionId::from_counter(u64::MAX - 1);
        let target = self.arm_target(&session)?;
        if let Err(error) = self.commit(&session, text) {
            self.cancel(&session);
            return Err(error);
        }
        Ok(target)
    }

    /// Commits text only when the context armed for this session still has focus.
    ///
    /// # Errors
    ///
    /// Returns an error for focus changes, secure contexts, session mismatch, or
    /// addon transport failure. Callers must not silently fall back after this.
    pub fn commit(self, session: &SessionId, text: &str) -> Result<(), VoxError> {
        let response = request(&[b"COMMIT", session.as_str().as_bytes(), text.as_bytes()])?;
        expect_ok(&response)
    }

    /// Clears any armed input context for the session.
    pub fn cancel(self, session: &SessionId) {
        let _result = request(&[b"CANCEL", session.as_str().as_bytes()]);
    }
}

fn parse_arm_response(response: &[u8]) -> Result<FcitxTarget, VoxError> {
    expect_ok(response)?;
    let mut fields = response.split(|byte| *byte == 0);
    let _status = fields.next();
    let _action = fields.next();
    let program = parse_identity_field(fields.next())?;
    let frontend = parse_identity_field(fields.next())?;
    Ok(FcitxTarget { program, frontend })
}

fn parse_identity_field(field: Option<&[u8]>) -> Result<String, VoxError> {
    match field {
        Some(value) => std::str::from_utf8(value)
            .map(|value| {
                if value.is_empty() {
                    "unknown".to_owned()
                } else {
                    value.to_owned()
                }
            })
            .map_err(|_| {
                VoxError::new(
                    ErrorCategory::Protocol,
                    "fcitx.invalid_response",
                    "Fcitx bridge response contains invalid UTF-8",
                )
            }),
        None => Ok("unknown".to_owned()),
    }
}

fn request(parts: &[&[u8]]) -> Result<Vec<u8>, VoxError> {
    let runtime = runtime_dir()?;
    fs::create_dir_all(&runtime).map_err(transport_io)?;
    let client_path = runtime.join(format!(
        "fcitx-client-{}-{}.sock",
        std::process::id(),
        CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let socket = UnixDatagram::bind(&client_path).map_err(transport_io)?;
    let _guard = SocketPathGuard(client_path);
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(transport_io)?;
    let message = parts
        .iter()
        .enumerate()
        .fold(Vec::new(), |mut message, (index, part)| {
            if index > 0 {
                message.push(0);
            }
            message.extend_from_slice(part);
            message
        });
    socket
        .send_to(&message, runtime.join("fcitx.sock"))
        .map_err(transport_io)?;
    let mut response = vec![0_u8; 4 * 1024];
    let size = socket.recv(&mut response).map_err(transport_io)?;
    response.truncate(size);
    Ok(response)
}

fn expect_ok(response: &[u8]) -> Result<(), VoxError> {
    let mut fields = response.split(|byte| *byte == 0);
    if fields.next() == Some(b"OK".as_slice()) {
        return Ok(());
    }
    let code = fields
        .next()
        .and_then(|value| std::str::from_utf8(value).ok())
        .unwrap_or("bridge-error");
    let category = match code {
        "secure-context" => ErrorCategory::Permission,
        "no-focused-context" | "focus-changed" => ErrorCategory::InvalidState,
        "session-mismatch" | "empty-text" => ErrorCategory::Protocol,
        _ => ErrorCategory::Unavailable,
    };
    Err(VoxError::new(
        category,
        "fcitx.bridge_rejected",
        format!("Fcitx bridge rejected request: {code}"),
    ))
}

fn runtime_dir() -> Result<PathBuf, VoxError> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("voxtype"))
        .ok_or_else(|| {
            VoxError::new(
                ErrorCategory::Unavailable,
                "fcitx.runtime_unavailable",
                "XDG_RUNTIME_DIR is unavailable",
            )
        })
}

fn transport_io(error: io::Error) -> VoxError {
    let message = error.to_string();
    drop(error);
    VoxError::new(
        ErrorCategory::Unavailable,
        "fcitx.transport_failed",
        message,
    )
}

struct SocketPathGuard(PathBuf);

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        let _result = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_secure_context_error() {
        let error = expect_ok(b"ERR\0secure-context").expect_err("must reject");
        assert_eq!(error.category(), ErrorCategory::Permission);
    }

    #[test]
    fn accepts_ok_response() {
        expect_ok(b"OK\0armed").expect("OK response");
    }

    #[test]
    fn parses_armed_target() {
        let target = parse_arm_response(b"OK\0armed\0kate\0wayland").expect("target response");
        assert_eq!(target.program, "kate");
        assert_eq!(target.frontend, "wayland");
    }

    #[test]
    fn normalizes_empty_target_identity() {
        let target = parse_arm_response(b"OK\0armed\0\0wayland").expect("target response");
        assert_eq!(target.program, "unknown");
        assert_eq!(target.frontend, "wayland");
    }

    #[test]
    fn rejects_empty_diagnostic_commit() {
        let error = FcitxBridge
            .commit_test("  ")
            .expect_err("empty diagnostic commit must fail locally");
        assert_eq!(error.code(), "fcitx.empty_test");
    }
}
