//! Strict provider-session lifecycle independent of the WebSocket implementation.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use voxtype_core::{AudioAcceptance, ErrorCategory};
use voxtype_provider_common::SecretString;
use zeroize::Zeroizing;

use crate::{
    FrameState, RecognitionEvent, decode_response, encode_request, parse_recognition_event,
};

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_SESSION_JSON_BYTES: usize = 256 * 1024;
const MAX_OPUS_PACKET_BYTES: usize = 1275;
const LAST_FRAME_MARKER_BYTES: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Disconnected,
    TaskStarting,
    TaskStarted,
    SessionStarting,
    Streaming,
    Finishing,
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputState {
    Open,
    AudioRequestPending,
    Finished,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOutcome {
    pub transcript: String,
    pub task_id: Option<String>,
    pub sent_audio_frames: u64,
    pub last_packet_number: Option<u64>,
    pub provider_vad_started: bool,
    pub provider_vad_finished: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    TaskStarted,
    SessionStarted,
    Recognition(RecognitionEvent),
    DuplicateIgnored,
    Finished(SessionOutcome),
}

/// Token-bearing request bytes that are cleared when dropped.
pub struct SecretRequest(Zeroizing<Vec<u8>>);

impl SecretRequest {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SecretRequest")
            .field(&format_args!("[{} redacted bytes]", self.0.len()))
            .finish()
    }
}

/// Pure protocol state machine used by the WebSocket I/O worker.
#[derive(Debug)]
pub struct DoubaoSessionProtocol {
    request_id: String,
    phase: SessionPhase,
    task_id: Option<String>,
    sent_audio_frames: u64,
    input_state: InputState,
    audio_acceptance: AudioAcceptance,
    last_packet_number: Option<u64>,
    final_transcript: Option<String>,
    provider_vad_started: bool,
    provider_vad_finished: bool,
}

impl DoubaoSessionProtocol {
    /// Creates a disconnected protocol instance for one unique request ID.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-containing request IDs before any
    /// network activity.
    pub fn new(request_id: String) -> Result<Self, SessionProtocolError> {
        if request_id.is_empty()
            || request_id.len() > MAX_REQUEST_ID_BYTES
            || request_id.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(SessionProtocolError::new(
                "doubao.invalid_request_id",
                "Doubao request ID is invalid",
            ));
        }
        Ok(Self {
            request_id,
            phase: SessionPhase::Disconnected,
            task_id: None,
            sent_audio_frames: 0,
            input_state: InputState::Open,
            audio_acceptance: AudioAcceptance::NotAccepted,
            last_packet_number: None,
            final_transcript: None,
            provider_vad_started: false,
            provider_vad_finished: false,
        })
    }

    #[must_use]
    pub const fn phase(&self) -> SessionPhase {
        self.phase
    }

    #[must_use]
    pub const fn audio_acceptance(&self) -> AudioAcceptance {
        self.audio_acceptance
    }

    #[must_use]
    pub const fn sent_audio_frames(&self) -> u64 {
        self.sent_audio_frames
    }

    /// Begins the remote task. Audio has not been accepted at this phase.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle error unless the protocol is disconnected.
    pub fn start_task(
        &mut self,
        token: &SecretString,
    ) -> Result<SecretRequest, SessionProtocolError> {
        self.require_phase(SessionPhase::Disconnected, "doubao.task_already_started")?;
        self.phase = SessionPhase::TaskStarting;
        Ok(SecretRequest(Zeroizing::new(encode_request(
            token.expose(),
            "StartTask",
            &[],
            &[],
            &self.request_id,
            None,
        ))))
    }

    /// Builds the bounded `StartSession` message after `TaskStarted`.
    ///
    /// # Errors
    ///
    /// Rejects an invalid phase or a malformed/non-object/oversized JSON body.
    pub fn start_session(
        &mut self,
        token: &SecretString,
        session_json: &[u8],
    ) -> Result<SecretRequest, SessionProtocolError> {
        self.require_phase(SessionPhase::TaskStarted, "doubao.task_not_started")?;
        if session_json.len() > MAX_SESSION_JSON_BYTES
            || !serde_json::from_slice::<serde_json::Value>(session_json)
                .ok()
                .is_some_and(|value| value.is_object())
        {
            return Err(self.fail(
                "doubao.invalid_session_json",
                "Doubao StartSession payload must be a bounded JSON object",
            ));
        }
        self.phase = SessionPhase::SessionStarting;
        Ok(SecretRequest(Zeroizing::new(encode_request(
            token.expose(),
            "StartSession",
            session_json,
            &[],
            &self.request_id,
            None,
        ))))
    }

    /// Builds one `TaskRequest` for a real independently encoded Opus packet.
    ///
    /// # Errors
    ///
    /// Enforces first/middle frame ordering and rejects empty or oversized
    /// packets. The caller must invoke [`Self::confirm_audio_sent`] only after
    /// the complete binary message has been written to the socket.
    pub fn audio_request(
        &mut self,
        timestamp_millis: u64,
        opus_packet: &[u8],
    ) -> Result<Vec<u8>, SessionProtocolError> {
        self.require_streaming_input()?;
        if self.input_state == InputState::AudioRequestPending {
            return Err(self.fail(
                "doubao.audio_request_pending",
                "Doubao audio request must be confirmed before building another frame",
            ));
        }
        if opus_packet.is_empty() || opus_packet.len() > MAX_OPUS_PACKET_BYTES {
            return Err(self.fail(
                "doubao.invalid_opus_packet",
                "Doubao audio packet is empty or oversized",
            ));
        }
        let state = if self.sent_audio_frames == 0 {
            FrameState::First
        } else {
            FrameState::Middle
        };
        let timestamp_json = timestamp_json(timestamp_millis);
        let request = encode_request(
            "",
            "TaskRequest",
            &timestamp_json,
            opus_packet,
            &self.request_id,
            Some(state),
        );
        self.input_state = InputState::AudioRequestPending;
        Ok(request)
    }

    /// Confirms that the pending real audio request was fully written.
    ///
    /// This is the precise boundary at which automatic cross-provider replay
    /// may no longer be safe.
    ///
    /// # Errors
    ///
    /// Rejects confirmation outside streaming or without a pending request.
    pub fn confirm_audio_sent(&mut self) -> Result<(), SessionProtocolError> {
        self.require_phase(SessionPhase::Streaming, "doubao.session_not_streaming")?;
        if self.input_state != InputState::AudioRequestPending {
            return Err(self.fail(
                "doubao.no_pending_audio_request",
                "Doubao has no pending audio request to confirm",
            ));
        }
        self.input_state = InputState::Open;
        self.sent_audio_frames = self.sent_audio_frames.saturating_add(1);
        self.audio_acceptance = AudioAcceptance::PossiblyAccepted;
        Ok(())
    }

    /// Records that a pending audio message may have been partially written.
    ///
    /// The frame is not counted as fully sent, but replay becomes unsafe. This
    /// is used when WebSocket flush fails after accepting the message into its
    /// internal write buffer.
    ///
    /// # Errors
    ///
    /// Rejects calls without a pending real audio request.
    pub fn mark_audio_write_ambiguous(&mut self) -> Result<(), SessionProtocolError> {
        self.require_phase(SessionPhase::Streaming, "doubao.session_not_streaming")?;
        if self.input_state != InputState::AudioRequestPending {
            return Err(self.fail(
                "doubao.no_pending_audio_request",
                "Doubao has no pending audio request to mark ambiguous",
            ));
        }
        self.audio_acceptance = AudioAcceptance::PossiblyAccepted;
        Ok(())
    }

    /// Builds the observed 100-byte zero last-frame marker.
    ///
    /// # Errors
    ///
    /// Rejects an empty session, duplicate finish, or invalid lifecycle phase.
    pub fn finish_audio(&mut self, timestamp_millis: u64) -> Result<Vec<u8>, SessionProtocolError> {
        self.require_streaming_input()?;
        if self.input_state == InputState::AudioRequestPending {
            return Err(self.fail(
                "doubao.audio_request_pending",
                "Doubao audio request must be confirmed before finishing input",
            ));
        }
        if self.sent_audio_frames == 0 {
            return Err(self.fail(
                "doubao.no_audio_frames",
                "Doubao session cannot finish before a real audio frame",
            ));
        }
        let timestamp_json = timestamp_json(timestamp_millis);
        let request = encode_request(
            "",
            "TaskRequest",
            &timestamp_json,
            &[0_u8; LAST_FRAME_MARKER_BYTES],
            &self.request_id,
            Some(FrameState::Last),
        );
        self.input_state = InputState::Finished;
        Ok(request)
    }

    /// Builds `FinishSession` after the last-frame marker.
    ///
    /// # Errors
    ///
    /// Rejects an invalid lifecycle phase or unfinished input.
    pub fn finish_session(
        &mut self,
        token: &SecretString,
    ) -> Result<SecretRequest, SessionProtocolError> {
        self.require_phase(SessionPhase::Streaming, "doubao.session_not_streaming")?;
        if self.input_state != InputState::Finished {
            return Err(self.fail(
                "doubao.audio_not_finished",
                "Doubao audio must finish before FinishSession",
            ));
        }
        self.phase = SessionPhase::Finishing;
        Ok(SecretRequest(Zeroizing::new(encode_request(
            token.expose(),
            "FinishSession",
            &[],
            &[],
            &self.request_id,
            None,
        ))))
    }

    /// Consumes one binary provider response and advances the lifecycle.
    ///
    /// # Errors
    ///
    /// Rejects malformed envelopes, mismatched request IDs, non-zero status,
    /// invalid phase transitions, remote failure messages, malformed result
    /// JSON, and `SessionFinished` without a non-empty final transcript.
    pub fn handle_binary(
        &mut self,
        message: &[u8],
    ) -> Result<Option<SessionEvent>, SessionProtocolError> {
        if matches!(
            self.phase,
            SessionPhase::Finished
                | SessionPhase::Failed
                | SessionPhase::Cancelled
                | SessionPhase::Disconnected
        ) {
            return Err(SessionProtocolError::new(
                "doubao.session_not_receiving",
                "Doubao session is not accepting provider responses",
            ));
        }
        let response = decode_response(message).map_err(|_| {
            self.fail(
                "doubao.invalid_response_envelope",
                "Doubao returned a malformed response envelope",
            )
        })?;
        if response.request_id != self.request_id {
            return Err(self.fail(
                "doubao.response_request_mismatch",
                "Doubao response request ID does not match the active session",
            ));
        }
        if !response.service.is_empty() && response.service != "ASR" {
            return Err(self.fail(
                "doubao.response_service_mismatch",
                "Doubao response service does not match ASR",
            ));
        }
        if response.status_code != 0 {
            if self.phase == SessionPhase::TaskStarting && auth_like_response(&response) {
                return Err(self.fail_with_category(
                    ErrorCategory::Authentication,
                    "doubao.start_task_auth_failed",
                    "Doubao rejected the StartTask credential",
                ));
            }
            return Err(self.fail(
                "doubao.remote_status_failed",
                "Doubao returned a non-zero protocol status",
            ));
        }
        match response.message_type.as_str() {
            "TaskFailed"
                if self.phase == SessionPhase::TaskStarting && auth_like_response(&response) =>
            {
                Err(self.fail_with_category(
                    ErrorCategory::Authentication,
                    "doubao.start_task_auth_failed",
                    "Doubao rejected the StartTask credential",
                ))
            }
            "TaskFailed" | "SessionFailed" => Err(self.fail(
                "doubao.remote_session_failed",
                "Doubao rejected the recognition session",
            )),
            "TaskStarted" => {
                self.require_phase(SessionPhase::TaskStarting, "doubao.unexpected_task_started")?;
                if !response.task_id.is_empty() {
                    self.task_id = Some(response.task_id);
                }
                self.phase = SessionPhase::TaskStarted;
                Ok(Some(SessionEvent::TaskStarted))
            }
            "SessionStarted" => {
                self.require_phase(
                    SessionPhase::SessionStarting,
                    "doubao.unexpected_session_started",
                )?;
                self.phase = SessionPhase::Streaming;
                Ok(Some(SessionEvent::SessionStarted))
            }
            "SessionFinished" => self.handle_session_finished(),
            _ => self.handle_result(&response),
        }
    }

    /// Makes cancellation terminal. Late provider messages cannot revive the
    /// session or produce text for insertion.
    pub fn cancel(&mut self) {
        if !matches!(self.phase, SessionPhase::Finished | SessionPhase::Failed) {
            self.phase = SessionPhase::Cancelled;
        }
    }

    fn handle_result(
        &mut self,
        response: &crate::ResponseEnvelope,
    ) -> Result<Option<SessionEvent>, SessionProtocolError> {
        if response.result_json.is_empty() {
            return Ok(None);
        }
        if !matches!(
            self.phase,
            SessionPhase::Streaming | SessionPhase::Finishing
        ) {
            return Err(self.fail(
                "doubao.result_before_session_started",
                "Doubao returned recognition data before the session started",
            ));
        }
        let event = parse_recognition_event(response).map_err(|_| {
            self.fail(
                "doubao.invalid_result_json",
                "Doubao returned malformed recognition result JSON",
            )
        })?;
        let Some(event) = event else {
            return Ok(None);
        };
        if event.packet_number.is_some_and(|packet| {
            self.last_packet_number
                .is_some_and(|previous| packet <= previous)
        }) {
            return Ok(Some(SessionEvent::DuplicateIgnored));
        }
        if let Some(packet) = event.packet_number {
            self.last_packet_number = Some(packet);
        }
        self.provider_vad_started |= event.vad_started;
        self.provider_vad_finished |= event.vad_finished;
        if event.vad_started || event.text.is_some() || event.final_result {
            self.audio_acceptance = AudioAcceptance::Accepted;
        }
        if event.final_result {
            let text = event
                .text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| {
                    self.fail(
                        "doubao.empty_final_transcript",
                        "Doubao returned an empty final transcript",
                    )
                })?;
            self.final_transcript = Some(text.to_owned());
        }
        Ok(Some(SessionEvent::Recognition(event)))
    }

    fn handle_session_finished(&mut self) -> Result<Option<SessionEvent>, SessionProtocolError> {
        self.require_phase(
            SessionPhase::Finishing,
            "doubao.unexpected_session_finished",
        )?;
        let transcript = self.final_transcript.clone().ok_or_else(|| {
            self.fail(
                "doubao.missing_final_transcript",
                "Doubao finished the session without a final transcript",
            )
        })?;
        self.phase = SessionPhase::Finished;
        Ok(Some(SessionEvent::Finished(SessionOutcome {
            transcript,
            task_id: self.task_id.clone(),
            sent_audio_frames: self.sent_audio_frames,
            last_packet_number: self.last_packet_number,
            provider_vad_started: self.provider_vad_started,
            provider_vad_finished: self.provider_vad_finished,
        })))
    }

    fn require_streaming_input(&mut self) -> Result<(), SessionProtocolError> {
        self.require_phase(SessionPhase::Streaming, "doubao.session_not_streaming")?;
        if self.input_state == InputState::Finished {
            return Err(self.fail(
                "doubao.audio_already_finished",
                "Doubao audio input is already finished",
            ));
        }
        Ok(())
    }

    fn require_phase(
        &mut self,
        expected: SessionPhase,
        code: &'static str,
    ) -> Result<(), SessionProtocolError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(self.fail(code, "Doubao session lifecycle transition is invalid"))
        }
    }

    fn fail(&mut self, code: &'static str, message: &'static str) -> SessionProtocolError {
        self.fail_with_category(ErrorCategory::Protocol, code, message)
    }

    fn fail_with_category(
        &mut self,
        category: ErrorCategory,
        code: &'static str,
        message: &'static str,
    ) -> SessionProtocolError {
        if self.phase != SessionPhase::Cancelled {
            self.phase = SessionPhase::Failed;
        }
        SessionProtocolError::with_category(category, code, message)
    }
}

fn auth_like_response(response: &crate::ResponseEnvelope) -> bool {
    if matches!(response.status_code, 401 | 403) {
        return true;
    }
    let normalized: String = response
        .status_message
        .chars()
        .take(1024)
        .flat_map(char::to_lowercase)
        .collect();
    [
        "token",
        "auth",
        "credential",
        "app_key",
        "unauthorized",
        "forbidden",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn timestamp_json(timestamp_millis: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "extra": {},
        "timestamp_ms": timestamp_millis,
    }))
    .expect("serializing a u64 timestamp cannot fail")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionProtocolError {
    category: ErrorCategory,
    code: &'static str,
    message: &'static str,
}

impl SessionProtocolError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self::with_category(ErrorCategory::Protocol, code, message)
    }

    const fn with_category(
        category: ErrorCategory,
        code: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            category,
            code,
            message,
        }
    }

    #[must_use]
    pub const fn category(self) -> ErrorCategory {
        self.category
    }

    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }
}

impl Display for SessionProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for SessionProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_varint(output: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            output.push(u8::try_from(value & 0x7f).expect("seven bits") | 0x80);
            value >>= 7;
        }
        output.push(u8::try_from(value).expect("last varint byte"));
    }

    fn write_bytes(output: &mut Vec<u8>, field: u32, value: &[u8]) {
        write_varint(output, u64::from(field) << 3 | 2);
        write_varint(output, u64::try_from(value.len()).expect("fixture length"));
        output.extend_from_slice(value);
    }

    fn response(
        request_id: &str,
        kind: &str,
        packet: Option<u64>,
        result: Option<&str>,
    ) -> Vec<u8> {
        let mut output = Vec::new();
        write_bytes(&mut output, 1, request_id.as_bytes());
        write_bytes(&mut output, 2, b"task-fixture");
        write_bytes(&mut output, 3, b"ASR");
        write_bytes(&mut output, 4, kind.as_bytes());
        if let Some(result) = result {
            write_bytes(&mut output, 7, result.as_bytes());
        }
        if let Some(packet) = packet {
            write_varint(&mut output, u64::from(9_u32) << 3);
            write_varint(&mut output, packet);
        }
        output
    }

    fn ready_protocol() -> (DoubaoSessionProtocol, SecretString) {
        let token = SecretString::try_new("token-fixture".to_owned()).expect("token");
        let mut protocol =
            DoubaoSessionProtocol::new("request-fixture".to_owned()).expect("session");
        protocol.start_task(&token).expect("StartTask");
        assert_eq!(
            protocol
                .handle_binary(&response("request-fixture", "TaskStarted", None, None))
                .expect("TaskStarted"),
            Some(SessionEvent::TaskStarted)
        );
        protocol
            .start_session(&token, br#"{"audio_info":{"sample_rate":16000}}"#)
            .expect("StartSession");
        assert_eq!(
            protocol
                .handle_binary(&response("request-fixture", "SessionStarted", None, None))
                .expect("SessionStarted"),
            Some(SessionEvent::SessionStarted)
        );
        (protocol, token)
    }

    #[test]
    fn lifecycle_requires_final_before_session_finished() {
        let (mut protocol, token) = ready_protocol();
        let first = protocol
            .audio_request(1_000, &[1, 2, 3])
            .expect("first frame");
        assert_eq!(protocol.audio_acceptance(), AudioAcceptance::NotAccepted);
        protocol.confirm_audio_sent().expect("audio written");
        assert_eq!(
            protocol.audio_acceptance(),
            AudioAcceptance::PossiblyAccepted
        );
        assert!(!first.windows(2).any(|part| part == [0x12, 0x00]));
        protocol.finish_audio(1_020).expect("last marker");
        protocol.finish_session(&token).expect("FinishSession");

        let final_json = r#"{"extra":{"packet_number":2},"results":[{"text":" 完成 ","is_interim":false,"is_vad_finished":true}]}"#;
        let event = protocol
            .handle_binary(&response(
                "request-fixture",
                "Result",
                None,
                Some(final_json),
            ))
            .expect("final result")
            .expect("recognition event");
        assert!(matches!(event, SessionEvent::Recognition(_)));
        assert_eq!(protocol.audio_acceptance(), AudioAcceptance::Accepted);

        let finished = protocol
            .handle_binary(&response("request-fixture", "SessionFinished", None, None))
            .expect("SessionFinished")
            .expect("finished event");
        let SessionEvent::Finished(outcome) = finished else {
            panic!("expected finished outcome");
        };
        assert_eq!(outcome.transcript, " 完成 ");
        assert_eq!(outcome.sent_audio_frames, 1);
        assert_eq!(outcome.last_packet_number, Some(2));
        assert_eq!(protocol.phase(), SessionPhase::Finished);
    }

    #[test]
    fn token_requests_are_redacted_and_ambiguous_writes_block_replay() {
        let token = SecretString::try_new("token-fixture".to_owned()).expect("token");
        let mut initial =
            DoubaoSessionProtocol::new("request-fixture".to_owned()).expect("session");
        let start = initial.start_task(&token).expect("StartTask");
        assert!(!format!("{start:?}").contains("token-fixture"));
        assert!(!start.as_bytes().is_empty());

        let (mut protocol, _) = ready_protocol();
        protocol.audio_request(1_000, &[1]).expect("audio");
        protocol
            .mark_audio_write_ambiguous()
            .expect("ambiguous socket write");
        assert_eq!(
            protocol.audio_acceptance(),
            AudioAcceptance::PossiblyAccepted
        );
        assert_eq!(protocol.sent_audio_frames(), 0);

        let heartbeat = r#"{"extra":{"packet_number":1}}"#;
        protocol
            .handle_binary(&response(
                "request-fixture",
                "Result",
                None,
                Some(heartbeat),
            ))
            .expect("packet heartbeat");
        assert_eq!(
            protocol.audio_acceptance(),
            AudioAcceptance::PossiblyAccepted
        );
    }

    #[test]
    fn task_request_uses_observed_timestamp_payload() {
        let (mut protocol, _) = ready_protocol();
        let request = protocol
            .audio_request(123_456, &[1])
            .expect("audio request");
        assert!(
            request
                .windows(br#"{"extra":{},"timestamp_ms":123456}"#.len())
                .any(|window| window == br#"{"extra":{},"timestamp_ms":123456}"#)
        );
    }

    #[test]
    fn duplicate_packet_cannot_overwrite_newer_final() {
        let (mut protocol, token) = ready_protocol();
        protocol.audio_request(1_000, &[1]).expect("audio");
        protocol.confirm_audio_sent().expect("audio written");
        let final_json = r#"{"extra":{"packet_number":8},"results":[{"text":"最终","is_interim":false,"is_vad_finished":true}]}"#;
        protocol
            .handle_binary(&response(
                "request-fixture",
                "Result",
                None,
                Some(final_json),
            ))
            .expect("final");
        let stale_json = r#"{"extra":{"packet_number":7},"results":[{"text":"旧文本","is_interim":false,"is_vad_finished":true}]}"#;
        assert_eq!(
            protocol
                .handle_binary(&response(
                    "request-fixture",
                    "Result",
                    None,
                    Some(stale_json)
                ))
                .expect("stale packet"),
            Some(SessionEvent::DuplicateIgnored)
        );
        protocol.finish_audio(1_020).expect("last marker");
        protocol.finish_session(&token).expect("finish");
        let finished = protocol
            .handle_binary(&response("request-fixture", "SessionFinished", None, None))
            .expect("finished")
            .expect("outcome");
        assert!(
            matches!(finished, SessionEvent::Finished(SessionOutcome { transcript, .. }) if transcript == "最终")
        );
    }

    #[test]
    fn mismatched_request_and_empty_final_are_terminal_errors() {
        let (mut protocol, _) = ready_protocol();
        let error = protocol
            .handle_binary(&response("another-request", "Result", None, Some("{}")))
            .expect_err("request mismatch");
        assert_eq!(error.code(), "doubao.response_request_mismatch");
        assert_eq!(protocol.phase(), SessionPhase::Failed);

        let (mut protocol, _) = ready_protocol();
        protocol.audio_request(1_000, &[1]).expect("audio");
        protocol.confirm_audio_sent().expect("audio written");
        let empty_final = r#"{"results":[{"text":" ","is_interim":false,"is_vad_finished":true}]}"#;
        let error = protocol
            .handle_binary(&response(
                "request-fixture",
                "Result",
                None,
                Some(empty_final),
            ))
            .expect_err("empty final");
        assert_eq!(error.code(), "doubao.empty_final_transcript");
        assert_eq!(protocol.phase(), SessionPhase::Failed);
    }

    #[test]
    fn nonzero_status_and_explicit_failure_are_terminal() {
        let (mut protocol, _) = ready_protocol();
        let mut failed = response("request-fixture", "Result", None, None);
        write_varint(&mut failed, u64::from(5_u32) << 3);
        write_varint(&mut failed, 17);
        let error = protocol
            .handle_binary(&failed)
            .expect_err("non-zero status must fail");
        assert_eq!(error.code(), "doubao.remote_status_failed");
        assert_eq!(protocol.phase(), SessionPhase::Failed);

        let (mut protocol, _) = ready_protocol();
        let error = protocol
            .handle_binary(&response("request-fixture", "SessionFailed", None, None))
            .expect_err("explicit failure must fail");
        assert_eq!(error.code(), "doubao.remote_session_failed");
        assert_eq!(protocol.phase(), SessionPhase::Failed);
    }

    #[test]
    fn start_task_token_failure_is_authentication_without_raw_message() {
        let token = SecretString::try_new("token-fixture".to_owned()).expect("token");
        let mut protocol =
            DoubaoSessionProtocol::new("request-fixture".to_owned()).expect("session");
        protocol.start_task(&token).expect("StartTask");
        let mut failed = response("request-fixture", "TaskFailed", None, None);
        write_bytes(
            &mut failed,
            6,
            b"app_key token expired: provider-secret-detail",
        );
        let error = protocol
            .handle_binary(&failed)
            .expect_err("expired token must fail");
        assert_eq!(error.category(), ErrorCategory::Authentication);
        assert_eq!(error.code(), "doubao.start_task_auth_failed");
        assert!(!error.to_string().contains("provider-secret-detail"));
        assert_eq!(protocol.phase(), SessionPhase::Failed);
    }

    #[test]
    fn cancellation_is_terminal_and_session_finished_needs_final_text() {
        let (mut protocol, _token) = ready_protocol();
        protocol.audio_request(1_000, &[1]).expect("audio");
        protocol.confirm_audio_sent().expect("audio written");
        protocol.cancel();
        assert_eq!(protocol.phase(), SessionPhase::Cancelled);
        let error = protocol
            .handle_binary(&response("request-fixture", "Result", None, Some("{}")))
            .expect_err("late result after cancel");
        assert_eq!(error.code(), "doubao.session_not_receiving");
        assert_eq!(protocol.phase(), SessionPhase::Cancelled);

        let (mut protocol, token) = ready_protocol();
        protocol.audio_request(1_000, &[1]).expect("audio");
        protocol.confirm_audio_sent().expect("audio written");
        protocol.finish_audio(1_020).expect("last marker");
        protocol.finish_session(&token).expect("finish");
        let error = protocol
            .handle_binary(&response("request-fixture", "SessionFinished", None, None))
            .expect_err("missing final");
        assert_eq!(error.code(), "doubao.missing_final_transcript");
        assert_eq!(protocol.phase(), SessionPhase::Failed);
    }

    #[test]
    fn multiple_results_aggregate_terminal_flags() {
        let response = crate::ResponseEnvelope {
            result_json: r#"{"results":[{"text":"候选","is_interim":false},{"text":"最后","is_vad_finished":true,"extra":{"nonstream_result":true}}]}"#
                .as_bytes()
                .to_vec(),
            ..crate::ResponseEnvelope::default()
        };
        let event = parse_recognition_event(&response)
            .expect("valid JSON")
            .expect("recognition event");
        assert_eq!(event.text.as_deref(), Some("最后"));
        assert!(!event.interim);
        assert!(event.vad_finished);
        assert!(event.final_result);
    }
}
