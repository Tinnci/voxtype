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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FcitxContext {
    pub target: FcitxTarget,
    pub generation: u64,
    pub cursor: usize,
    pub anchor: usize,
    pub capabilities: Vec<String>,
    pub truncated: bool,
    pub text: String,
}

impl FcitxContext {
    /// Returns the selected text, or up to 1200 characters before the cursor.
    #[must_use]
    pub fn review_text(&self) -> String {
        let start = self.cursor.min(self.anchor);
        let end = self.cursor.max(self.anchor);
        if start != end {
            return slice_characters(&self.text, start, end);
        }
        slice_characters(&self.text, self.cursor.saturating_sub(1_200), self.cursor)
    }
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

    /// Reads a bounded, non-sensitive snapshot from the focused input context.
    ///
    /// # Errors
    ///
    /// Returns an error when focus is unavailable, the field is sensitive, or
    /// the frontend does not provide a valid context response.
    pub fn context(self) -> Result<FcitxContext, VoxError> {
        let response = request(&[b"CONTEXT"])?;
        parse_context_response(&response)
    }

    /// Arms and dispatches a diagnostic commit to the currently focused context.
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
        let request_id = format!(
            "{}-{}",
            std::process::id(),
            CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let response = request_with_retry(&[
            b"COMMIT2",
            request_id.as_bytes(),
            session.as_str().as_bytes(),
            text.as_bytes(),
        ])?;
        match expect_dispatched_for(&response, &request_id) {
            Ok(()) => Ok(()),
            Err(error) if error.message().contains("unknown-command") => {
                let response = request(&[b"COMMIT", session.as_str().as_bytes(), text.as_bytes()])?;
                expect_dispatched(&response)
            }
            Err(error) => Err(error),
        }
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

fn parse_context_response(response: &[u8]) -> Result<FcitxContext, VoxError> {
    expect_ok(response)?;
    let mut fields = response.split(|byte| *byte == 0);
    let _status = fields.next();
    if fields.next() != Some(b"context".as_slice()) {
        return Err(invalid_response(
            "Fcitx bridge response is not a context snapshot",
        ));
    }
    let program = parse_identity_field(fields.next())?;
    let frontend = parse_identity_field(fields.next())?;
    let generation = parse_number(fields.next(), "context generation")?;
    let cursor = usize::try_from(parse_number(fields.next(), "cursor")?)
        .map_err(|_| invalid_response("Fcitx cursor is out of range"))?;
    let anchor = usize::try_from(parse_number(fields.next(), "anchor")?)
        .map_err(|_| invalid_response("Fcitx anchor is out of range"))?;
    let capabilities = parse_utf8(fields.next(), "capabilities")?
        .split(',')
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let truncated = match fields.next() {
        Some(b"0") => false,
        Some(b"1") => true,
        _ => return Err(invalid_response("Fcitx truncation flag is invalid")),
    };
    let text = parse_utf8(fields.next(), "surrounding text")?.to_owned();
    let characters = text.chars().count();
    if characters > 4_096 || cursor > characters || anchor > characters {
        return Err(invalid_response(
            "Fcitx surrounding text or cursor exceeds the bounded context",
        ));
    }
    Ok(FcitxContext {
        target: FcitxTarget { program, frontend },
        generation,
        cursor,
        anchor,
        capabilities,
        truncated,
        text,
    })
}

fn parse_number(field: Option<&[u8]>, name: &str) -> Result<u64, VoxError> {
    parse_utf8(field, name)?
        .parse()
        .map_err(|_| invalid_response(&format!("Fcitx {name} is invalid")))
}

fn parse_utf8<'a>(field: Option<&'a [u8]>, name: &str) -> Result<&'a str, VoxError> {
    let field = field.ok_or_else(|| invalid_response(&format!("Fcitx {name} is missing")))?;
    std::str::from_utf8(field)
        .map_err(|_| invalid_response(&format!("Fcitx {name} contains invalid UTF-8")))
}

fn invalid_response(message: &str) -> VoxError {
    VoxError::new(
        ErrorCategory::Protocol,
        "fcitx.invalid_response",
        message.to_owned(),
    )
}

fn slice_characters(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
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
    request_once(parts)
}

fn request_with_retry(parts: &[&[u8]]) -> Result<Vec<u8>, VoxError> {
    let runtime = runtime_dir()?;
    request_with_retry_at(parts, &runtime)
}

fn request_with_retry_at(parts: &[&[u8]], runtime: &std::path::Path) -> Result<Vec<u8>, VoxError> {
    let message = encode_message(parts)?;
    retry_message(&message, |message| request_message_at(message, runtime))
}

fn retry_message<T>(
    message: &[u8],
    mut operation: impl FnMut(&[u8]) -> Result<T, VoxError>,
) -> Result<T, VoxError> {
    match operation(message) {
        Ok(response) => Ok(response),
        Err(error) if error.code() == "fcitx.transport_failed" => operation(message),
        Err(error) => Err(error),
    }
}

fn request_once(parts: &[&[u8]]) -> Result<Vec<u8>, VoxError> {
    let runtime = runtime_dir()?;
    request_once_at(parts, &runtime)
}

fn request_once_at(parts: &[&[u8]], runtime: &std::path::Path) -> Result<Vec<u8>, VoxError> {
    let message = encode_message(parts)?;
    request_message_at(&message, runtime)
}

fn request_message_at(message: &[u8], runtime: &std::path::Path) -> Result<Vec<u8>, VoxError> {
    fs::create_dir_all(runtime).map_err(transport_io)?;
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
    socket
        .send_to(message, runtime.join("fcitx.sock"))
        .map_err(transport_io)?;
    let mut response = vec![0_u8; 20 * 1024];
    let size = socket.recv(&mut response).map_err(transport_io)?;
    response.truncate(size);
    Ok(response)
}

fn encode_message(parts: &[&[u8]]) -> Result<Vec<u8>, VoxError> {
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
    if message.len() > 60 * 1024 {
        return Err(VoxError::new(
            ErrorCategory::Protocol,
            "fcitx.message_too_large",
            "Fcitx bridge request exceeds the bounded datagram size",
        ));
    }
    Ok(message)
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

fn expect_dispatched(response: &[u8]) -> Result<(), VoxError> {
    expect_ok(response)?;
    let detail = response.split(|byte| *byte == 0).nth(1).unwrap_or_default();
    if matches!(detail, b"dispatched" | b"committed") {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::Protocol,
            "fcitx.dispatch_unconfirmed",
            "Fcitx bridge did not confirm dispatch to the input context",
        ))
    }
}

fn expect_dispatched_for(response: &[u8], request_id: &str) -> Result<(), VoxError> {
    expect_ok(response)?;
    let mut fields = response.split(|byte| *byte == 0);
    let _status = fields.next();
    let detail = fields.next().unwrap_or_default();
    let echoed_id = fields
        .next()
        .and_then(|value| std::str::from_utf8(value).ok());
    if matches!(detail, b"dispatched" | b"committed") && echoed_id == Some(request_id) {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::Protocol,
            "fcitx.dispatch_unconfirmed",
            "Fcitx bridge did not confirm dispatch for this request id",
        ))
    }
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
    fn commit_requires_final_dispatch_confirmation() {
        expect_dispatched(b"OK\0dispatched").expect("confirmed dispatch");
        expect_dispatched(b"OK\0committed").expect("accept previous addon during upgrade");
        let error = expect_dispatched(b"OK\0queued").expect_err("queued is not dispatched");
        assert_eq!(error.code(), "fcitx.dispatch_unconfirmed");
    }

    #[test]
    fn commit_ack_must_echo_request_id() {
        expect_dispatched_for(b"OK\0dispatched\0req-1", "req-1").expect("matching request id");
        let error = expect_dispatched_for(b"OK\0dispatched\0req-2", "req-1")
            .expect_err("mismatched request id");
        assert_eq!(error.code(), "fcitx.dispatch_unconfirmed");
    }

    #[test]
    fn rejects_oversized_bridge_message_before_send() {
        let text = vec![b'x'; 60 * 1024];
        let error = encode_message(&[b"PING", text.as_slice()]).expect_err("must reject");
        assert_eq!(error.code(), "fcitx.message_too_large");
    }

    #[test]
    fn transport_retry_reuses_the_same_commit_request_id() {
        let message =
            encode_message(&[b"COMMIT2", b"req-test", b"session-1", b"hello"]).expect("message");
        let mut attempts = Vec::new();
        let response = retry_message(&message, |attempt| {
            attempts.push(attempt.to_vec());
            if attempts.len() == 1 {
                Err(VoxError::new(
                    ErrorCategory::Unavailable,
                    "fcitx.transport_failed",
                    "simulated lost ACK",
                ))
            } else {
                Ok(b"OK\0dispatched\0req-test".to_vec())
            }
        })
        .expect("retry succeeds");
        expect_dispatched_for(&response, "req-test").expect("matching retry ack");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
    }

    #[test]
    fn parses_armed_target() {
        let target = parse_arm_response(b"OK\0armed\0kate\0wayland").expect("target response");
        assert_eq!(target.program, "kate");
        assert_eq!(target.frontend, "wayland");
    }

    #[test]
    fn parses_bounded_context_and_selection() {
        let response = encode_message(&[
            b"OK",
            b"context",
            b"kate",
            b"wayland",
            b"42",
            b"3",
            b"1",
            b"surrounding-text,multiline",
            b"0",
            "abc中".as_bytes(),
        ])
        .expect("response encoding");
        let context = parse_context_response(&response).expect("context response");
        assert_eq!(context.generation, 42);
        assert_eq!(context.cursor, 3);
        assert_eq!(context.anchor, 1);
        assert_eq!(context.review_text(), "bc");
        assert_eq!(context.text, "abc中");
        assert!(context.capabilities.contains(&"multiline".to_owned()));
    }

    #[test]
    fn context_without_selection_returns_text_before_cursor() {
        let context = FcitxContext {
            target: FcitxTarget {
                program: "kate".to_owned(),
                frontend: "wayland".to_owned(),
            },
            generation: 1,
            cursor: 4,
            anchor: 4,
            capabilities: vec!["surrounding-text".to_owned()],
            truncated: false,
            text: "前文内容后文".to_owned(),
        };
        assert_eq!(context.review_text(), "前文内容");
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
