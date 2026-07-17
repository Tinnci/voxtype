//! Clean-room protocol primitives for the unofficial Doubao ASR transport.
//!
//! This crate intentionally contains no Android identity constants, network
//! client, credentials, or copied reference-source code. It implements only
//! the independently documented protobuf wire envelope, 20 ms PCM framing,
//! and provider-event interpretation required by a future transport adapter.

use serde_json::Value;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use voxtype_provider_common::SecretString;

pub const PCM_FRAME_BYTES: usize = 640;
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BOOTSTRAP_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Eq, PartialEq)]
pub struct DeviceIdentity {
    pub client_udid: String,
    pub open_udid: String,
    pub cdid: String,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("client_udid", &"[redacted]")
            .field("open_udid", &"[redacted]")
            .field("cdid", &"[redacted]")
            .finish()
    }
}

impl DeviceIdentity {
    /// Validates caller-generated persistent identifiers without prescribing a
    /// proprietary generation algorithm.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, or control-character values.
    pub fn validate(&self) -> Result<(), BootstrapError> {
        for value in [&self.client_udid, &self.open_udid, &self.cdid] {
            if value.is_empty()
                || value.len() > 128
                || value.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err(BootstrapError("invalid persistent device identifier"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RegisteredDevice {
    pub device_id: String,
    pub install_id: String,
}

impl fmt::Debug for RegisteredDevice {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisteredDevice")
            .field("device_id", &"[redacted]")
            .field("install_id", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapError(&'static str);

impl Display for BootstrapError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for BootstrapError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameState {
    First = 1,
    Middle = 3,
    Last = 9,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResponseEnvelope {
    pub request_id: String,
    pub task_id: String,
    pub service: String,
    pub message_type: String,
    pub status_code: u64,
    pub status_message: String,
    pub result_json: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // Mirrors independent provider flags without conflating them.
pub struct RecognitionEvent {
    pub text: Option<String>,
    pub interim: bool,
    pub vad_started: bool,
    pub vad_finished: bool,
    pub final_result: bool,
    pub packet_number: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolError(&'static str);

impl Display for ProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for ProtocolError {}

/// Computes the observed uppercase MD5 compatibility header for the exact
/// settings request body. This is not treated as authentication.
#[must_use]
pub fn x_ss_stub(body: &[u8]) -> String {
    format!("{:X}", md5::compute(body))
}

/// Parses successful device registration identifiers from a bounded JSON
/// response. Numeric and string IDs are accepted for compatibility.
///
/// # Errors
///
/// Returns an error for oversized/malformed JSON or absent/non-positive IDs.
pub fn parse_device_registration(body: &[u8]) -> Result<RegisteredDevice, BootstrapError> {
    let value = parse_bootstrap_json(body)?;
    let device_id = identifier_at(&value, &["device_id"])
        .or_else(|| identifier_at(&value, &["data", "device_id"]))
        .ok_or(BootstrapError(
            "registration response has no positive device_id",
        ))?;
    let install_id = identifier_at(&value, &["install_id"])
        .or_else(|| identifier_at(&value, &["data", "install_id"]))
        .unwrap_or_default();
    Ok(RegisteredDevice {
        device_id,
        install_id,
    })
}

/// Extracts the ASR token from the observed settings response path and stores
/// it in the workspace's zeroizing secret type.
///
/// # Errors
///
/// Returns an error for oversized/malformed JSON, a missing token, or invalid
/// secret bytes.
pub fn parse_settings_token(body: &[u8]) -> Result<SecretString, BootstrapError> {
    let value = parse_bootstrap_json(body)?;
    let token = value
        .pointer("/data/settings/asr_config/app_key")
        .and_then(Value::as_str)
        .ok_or(BootstrapError("settings response has no ASR app_key"))?;
    SecretString::try_new(token.to_owned())
        .map_err(|_| BootstrapError("settings ASR app_key is invalid"))
}

fn parse_bootstrap_json(body: &[u8]) -> Result<Value, BootstrapError> {
    if body.len() > MAX_BOOTSTRAP_RESPONSE_BYTES {
        return Err(BootstrapError("Doubao bootstrap response is too large"));
    }
    serde_json::from_slice(body).map_err(|_| BootstrapError("invalid Doubao bootstrap JSON"))
}

fn identifier_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    let identifier = match current {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    (!identifier.is_empty() && identifier != "0").then_some(identifier)
}

/// Encodes the observed request envelope without depending on generated
/// protobuf code. Empty JSON/audio fields are omitted.
#[must_use]
pub fn encode_request(
    token: &str,
    method: &str,
    json_payload: &[u8],
    audio: &[u8],
    request_id: &str,
    frame_state: Option<FrameState>,
) -> Vec<u8> {
    let capacity = token
        .len()
        .saturating_add(method.len())
        .saturating_add(json_payload.len())
        .saturating_add(audio.len())
        .saturating_add(request_id.len())
        .saturating_add(32);
    let mut output = Vec::with_capacity(capacity);
    write_bytes(&mut output, 2, token.as_bytes());
    write_bytes(&mut output, 3, b"ASR");
    write_bytes(&mut output, 5, method.as_bytes());
    if !json_payload.is_empty() {
        write_bytes(&mut output, 6, json_payload);
    }
    if !audio.is_empty() {
        write_bytes(&mut output, 7, audio);
    }
    write_bytes(&mut output, 8, request_id.as_bytes());
    if let Some(state) = frame_state {
        write_varint_field(&mut output, 9, state as u64);
    }
    output
}

/// Decodes the documented response fields and skips valid unknown protobuf
/// fields, allowing compatible server additions.
///
/// # Errors
///
/// Returns an error for oversized, truncated, malformed, or unsupported wire
/// data.
pub fn decode_response(message: &[u8]) -> Result<ResponseEnvelope, ProtocolError> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError("doubao response exceeds the size limit"));
    }
    let mut cursor = 0;
    let mut response = ResponseEnvelope::default();
    while cursor < message.len() {
        let key = read_varint(message, &mut cursor)?;
        let field = u32::try_from(key >> 3).map_err(|_| ProtocolError("invalid field number"))?;
        let wire = u8::try_from(key & 7).map_err(|_| ProtocolError("invalid wire type"))?;
        match (field, wire) {
            (1, 2) => response.request_id = read_string(message, &mut cursor)?,
            (2, 2) => response.task_id = read_string(message, &mut cursor)?,
            (3, 2) => response.service = read_string(message, &mut cursor)?,
            (4, 2) => response.message_type = read_string(message, &mut cursor)?,
            (5, 0) => response.status_code = read_varint(message, &mut cursor)?,
            (6, 2) => response.status_message = read_string(message, &mut cursor)?,
            (7, 2) => response.result_json = read_bytes(message, &mut cursor)?.to_vec(),
            (_, _) => skip_field(message, &mut cursor, wire)?,
        }
    }
    Ok(response)
}

/// Interprets the provider result JSON without retaining raw diagnostic data.
///
/// # Errors
///
/// Returns an error when the provider result is not valid JSON.
pub fn parse_recognition_event(
    response: &ResponseEnvelope,
) -> Result<Option<RecognitionEvent>, ProtocolError> {
    if response.result_json.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&response.result_json)
        .map_err(|_| ProtocolError("invalid Doubao result JSON"))?;
    let extra = value.get("extra").unwrap_or(&Value::Null);
    let vad_started = extra
        .get("vad_start")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let packet_number = extra.get("packet_number").and_then(Value::as_u64);
    let Some(result) = value
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.last())
    else {
        return Ok(Some(RecognitionEvent {
            text: None,
            interim: true,
            vad_started,
            vad_finished: false,
            final_result: false,
            packet_number,
        }));
    };
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let interim = result
        .get("is_interim")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let vad_finished = result
        .get("is_vad_finished")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let nonstream = result
        .get("extra")
        .and_then(|value| value.get("nonstream_result"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Some(RecognitionEvent {
        text,
        interim,
        vad_started,
        vad_finished,
        final_result: nonstream || (!interim && vad_finished),
        packet_number,
    }))
}

/// Collects arbitrarily sized PCM chunks into exact 20 ms frames. A final
/// partial frame is zero-padded explicitly rather than silently dropped.
#[derive(Clone, Debug, Default)]
pub struct Pcm20msFramer {
    pending: Vec<u8>,
}

impl Pcm20msFramer {
    /// # Panics
    ///
    /// Does not panic: the frame copy is guarded by the pending-length check.
    #[must_use]
    pub fn push(&mut self, pcm: &[u8]) -> Vec<[u8; PCM_FRAME_BYTES]> {
        self.pending.extend_from_slice(pcm);
        let mut frames = Vec::new();
        while self.pending.len() >= PCM_FRAME_BYTES {
            let mut frame = [0_u8; PCM_FRAME_BYTES];
            frame.copy_from_slice(&self.pending[..PCM_FRAME_BYTES]);
            frames.push(frame);
            self.pending.drain(..PCM_FRAME_BYTES);
        }
        frames
    }

    #[must_use]
    pub fn finish_padded(&mut self) -> Option<[u8; PCM_FRAME_BYTES]> {
        if self.pending.is_empty() {
            return None;
        }
        let mut frame = [0_u8; PCM_FRAME_BYTES];
        frame[..self.pending.len()].copy_from_slice(&self.pending);
        self.pending.clear();
        Some(frame)
    }

    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }
}

fn write_bytes(output: &mut Vec<u8>, field: u32, value: &[u8]) {
    write_varint(output, u64::from(field) << 3 | 2);
    write_varint(output, u64::try_from(value.len()).unwrap_or(u64::MAX));
    output.extend_from_slice(value);
}

fn write_varint_field(output: &mut Vec<u8>, field: u32, value: u64) {
    write_varint(output, u64::from(field) << 3);
    write_varint(output, value);
}

fn write_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push(u8::try_from(value & 0x7f).unwrap_or_default() | 0x80);
        value >>= 7;
    }
    output.push(u8::try_from(value).unwrap_or_default());
}

fn read_varint(message: &[u8], cursor: &mut usize) -> Result<u64, ProtocolError> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *message
            .get(*cursor)
            .ok_or(ProtocolError("truncated protobuf varint"))?;
        *cursor = cursor.saturating_add(1);
        if shift == 63 && byte > 1 {
            return Err(ProtocolError("protobuf varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ProtocolError("protobuf varint is too long"))
}

fn read_bytes<'a>(message: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], ProtocolError> {
    let length = usize::try_from(read_varint(message, cursor)?)
        .map_err(|_| ProtocolError("protobuf field length overflows usize"))?;
    let end = cursor
        .checked_add(length)
        .ok_or(ProtocolError("protobuf field length overflow"))?;
    let value = message
        .get(*cursor..end)
        .ok_or(ProtocolError("truncated protobuf bytes field"))?;
    *cursor = end;
    Ok(value)
}

fn read_string(message: &[u8], cursor: &mut usize) -> Result<String, ProtocolError> {
    String::from_utf8(read_bytes(message, cursor)?.to_vec())
        .map_err(|_| ProtocolError("protobuf string is not UTF-8"))
}

fn skip_field(message: &[u8], cursor: &mut usize, wire: u8) -> Result<(), ProtocolError> {
    match wire {
        0 => {
            let _ = read_varint(message, cursor)?;
        }
        1 => advance(message, cursor, 8)?,
        2 => {
            let _ = read_bytes(message, cursor)?;
        }
        5 => advance(message, cursor, 4)?,
        _ => return Err(ProtocolError("unsupported protobuf wire type")),
    }
    Ok(())
}

fn advance(message: &[u8], cursor: &mut usize, count: usize) -> Result<(), ProtocolError> {
    let end = cursor
        .checked_add(count)
        .ok_or(ProtocolError("protobuf cursor overflow"))?;
    if end > message.len() {
        return Err(ProtocolError("truncated protobuf fixed-width field"));
    }
    *cursor = end;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_parses_ids_and_zeroizing_token() {
        let registered =
            parse_device_registration(br#"{"data":{"device_id":123456,"install_id":"654321"}}"#)
                .expect("registered device");
        assert_eq!(registered.device_id, "123456");
        assert_eq!(registered.install_id, "654321");
        assert!(!format!("{registered:?}").contains("123456"));

        let token = parse_settings_token(
            br#"{"data":{"settings":{"asr_config":{"app_key":"secret-token"}}}}"#,
        )
        .expect("settings token");
        assert_eq!(token.expose(), "secret-token");
        assert_eq!(format!("{token:?}"), "SecretString([redacted])");
    }

    #[test]
    fn bootstrap_rejects_missing_ids_tokens_and_control_bytes() {
        assert!(parse_device_registration(br#"{"device_id":0}"#).is_err());
        assert!(parse_settings_token(br#"{"data":{}}"#).is_err());
        assert!(
            parse_settings_token(
                b"{\"data\":{\"settings\":{\"asr_config\":{\"app_key\":\"bad\\nkey\"}}}}"
            )
            .is_err()
        );
        let identity = DeviceIdentity {
            client_udid: "client".to_owned(),
            open_udid: "bad\nvalue".to_owned(),
            cdid: "cdid".to_owned(),
        };
        assert!(identity.validate().is_err());
        assert!(!format!("{identity:?}").contains("\"client\""));
    }

    #[test]
    fn settings_stub_is_uppercase_md5_of_exact_body() {
        assert_eq!(x_ss_stub(b"body=null"), "46C03B52742B3F2615A3ABDF1636B754");
        assert_ne!(x_ss_stub(b"body=null"), x_ss_stub(b"body=null\n"));
    }

    #[test]
    fn request_envelope_matches_independent_golden_bytes() {
        let encoded = encode_request("t", "M", &[], &[], "r", Some(FrameState::First));
        assert_eq!(
            encoded,
            [
                0x12, 0x01, b't', 0x1a, 0x03, b'A', b'S', b'R', 0x2a, 0x01, b'M', 0x42, 0x01, b'r',
                0x48, 0x01,
            ]
        );
    }

    #[test]
    fn response_decoder_skips_unknown_fields() {
        let json = r#"{"extra":{"packet_number":7},"results":[{"text":"你好","is_interim":false,"is_vad_finished":true}]}"#.as_bytes();
        let mut message = Vec::new();
        write_bytes(&mut message, 1, b"request-1");
        write_bytes(&mut message, 4, b"SessionFinished");
        write_varint_field(&mut message, 5, 0);
        write_varint_field(&mut message, 9, 123);
        write_bytes(&mut message, 7, json);
        let response = decode_response(&message).expect("response");
        assert_eq!(response.request_id, "request-1");
        assert_eq!(response.message_type, "SessionFinished");
        let event = parse_recognition_event(&response)
            .expect("event JSON")
            .expect("recognition event");
        assert_eq!(event.text.as_deref(), Some("你好"));
        assert!(event.final_result);
        assert_eq!(event.packet_number, Some(7));
    }

    #[test]
    fn nonstream_result_is_final_without_vad_finished() {
        let response = ResponseEnvelope {
            result_json: r#"{"results":[{"text":"完成","is_interim":true,"extra":{"nonstream_result":true}}]}"#.as_bytes().to_vec(),
            ..ResponseEnvelope::default()
        };
        assert!(
            parse_recognition_event(&response)
                .expect("event")
                .expect("result")
                .final_result
        );
    }

    #[test]
    fn pcm_chunks_are_reassembled_and_final_frame_is_padded() {
        let mut framer = Pcm20msFramer::default();
        assert!(framer.push(&[1; 100]).is_empty());
        let completed = framer.push(&[2; 700]);
        assert_eq!(completed.len(), 1);
        assert_eq!(&completed[0][..100], &[1; 100]);
        assert_eq!(&completed[0][100..], &[2; 540]);
        assert_eq!(framer.pending_bytes(), 160);
        let final_frame = framer.finish_padded().expect("padded frame");
        assert_eq!(&final_frame[..160], &[2; 160]);
        assert!(final_frame[160..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        assert!(decode_response(&[0x3a, 0x05, 1]).is_err());
        let mut oversized_varint = vec![0x28];
        oversized_varint.extend_from_slice(&[0x80; 11]);
        assert!(decode_response(&oversized_varint).is_err());
        assert!(decode_response(&[0x0b]).is_err());
    }
}
