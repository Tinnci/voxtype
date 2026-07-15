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
        let response = request(&[b"ARM", session.as_str().as_bytes()])?;
        expect_ok(&response)
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
}
