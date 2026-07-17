//! Clean-room protocol primitives for the unofficial Doubao ASR transport.
//!
//! This crate intentionally contains no Android identity constants, production
//! endpoints, or copied reference-source code. It implements independently
//! documented bootstrap HTTP, protobuf, 20 ms PCM/Opus framing, and provider
//! event primitives for a future session transport.

pub mod opus_codec;

use serde_json::Value;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use voxtype_core::{ErrorCategory, VoxError};
use voxtype_provider_common::{
    CancellationToken, DEFAULT_MAX_RESPONSE_BYTES, SecretString, escape_curl_config,
    execute_curl_cancellable,
};

pub const PCM_FRAME_BYTES: usize = 640;
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BOOTSTRAP_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_REGISTRATION_BODY_BYTES: usize = 256 * 1024;
const MAX_QUERY_PARAMETERS: usize = 64;
const SETTINGS_BODY: &[u8] = b"body=null";

/// HTTP endpoints and timeouts for the unofficial credential bootstrap.
///
/// Endpoints intentionally have no built-in production defaults: choosing the
/// observed private service and client identity is a distribution policy
/// decision made above this protocol crate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapHttpConfig {
    pub registration_endpoint: String,
    pub settings_endpoint: String,
    pub timeout_seconds: u64,
}

/// Caller-supplied client metadata shared by registration and settings calls.
///
/// Query values may include installation metadata and are therefore redacted
/// from `Debug`. Reserved identity keys are added by this crate and cannot be
/// overridden by the caller.
#[derive(Clone, Eq, PartialEq)]
pub struct BootstrapRequestContext {
    pub user_agent: String,
    pub common_query: Vec<(String, String)>,
}

impl fmt::Debug for BootstrapRequestContext {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapRequestContext")
            .field("user_agent", &"[redacted]")
            .field(
                "common_query",
                &format_args!("[{} redacted pairs]", self.common_query.len()),
            )
            .finish()
    }
}

/// Bounded serialized registration document supplied by the licensed client
/// identity layer. Keeping its schema out of this crate prevents an observed
/// Android identity template from becoming part of the provider-neutral API.
#[derive(Clone, Eq, PartialEq)]
pub struct RegistrationDocument(Vec<u8>);

impl fmt::Debug for RegistrationDocument {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RegistrationDocument")
            .field(&format_args!("[{} redacted bytes]", self.0.len()))
            .finish()
    }
}

impl RegistrationDocument {
    /// Serializes a caller-owned JSON object under a strict size bound.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not an object, cannot be serialized,
    /// or exceeds the registration request limit.
    pub fn from_json(value: &Value) -> Result<Self, BootstrapError> {
        if !value.is_object() {
            return Err(BootstrapError(
                "registration document must be a JSON object",
            ));
        }
        let bytes = serde_json::to_vec(value)
            .map_err(|_| BootstrapError("could not serialize registration document"))?;
        if bytes.len() > MAX_REGISTRATION_BODY_BYTES {
            return Err(BootstrapError("registration document is too large"));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

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

/// Registers one persistent device identity through the bounded, cancellable
/// system curl transport.
///
/// The full URL and body are written through curl's private stdin config so
/// device identifiers do not appear in the process argument list.
///
/// # Errors
///
/// Returns a stable configuration, cancellation, connection, HTTP, or protocol
/// error. Raw response bodies, query strings, and identifiers are never copied
/// into the error message.
pub fn register_device_http(
    config: &BootstrapHttpConfig,
    context: &BootstrapRequestContext,
    document: &RegistrationDocument,
    cancellation: &CancellationToken,
) -> Result<RegisteredDevice, VoxError> {
    validate_bootstrap_config(config, context)?;
    let url = bootstrap_url(&config.registration_endpoint, &context.common_query, &[])?;
    let private_config = curl_post_config(
        &url,
        &context.user_agent,
        "application/json",
        &document.0,
        None,
    );
    let response = execute_curl_cancellable(
        std::iter::empty::<&str>(),
        config.timeout_seconds,
        private_config.as_bytes(),
        DEFAULT_MAX_RESPONSE_BYTES,
        cancellation,
    )
    .map_err(|error| error.into_vox_error("doubao.registration_http_failed"))?;
    ensure_not_cancelled(cancellation)?;
    parse_device_registration(&response.body).map_err(|_| {
        bootstrap_error(
            ErrorCategory::Protocol,
            "doubao.invalid_registration_response",
            "Doubao registration response did not contain valid identifiers",
        )
    })
}

/// Fetches the refreshable ASR token for a registered device.
///
/// # Errors
///
/// Returns a stable configuration, cancellation, connection, HTTP, or protocol
/// error without exposing the device identifier or returned token.
pub fn fetch_settings_token_http(
    config: &BootstrapHttpConfig,
    context: &BootstrapRequestContext,
    registered: &RegisteredDevice,
    cancellation: &CancellationToken,
) -> Result<SecretString, VoxError> {
    validate_bootstrap_config(config, context)?;
    validate_registered_device(registered)?;
    let url = bootstrap_url(
        &config.settings_endpoint,
        &context.common_query,
        &[("device_id", registered.device_id.as_str())],
    )?;
    let stub = x_ss_stub(SETTINGS_BODY);
    let private_config = curl_post_config(
        &url,
        &context.user_agent,
        "application/x-www-form-urlencoded",
        SETTINGS_BODY,
        Some(("x-ss-stub", &stub)),
    );
    let response = execute_curl_cancellable(
        std::iter::empty::<&str>(),
        config.timeout_seconds,
        private_config.as_bytes(),
        DEFAULT_MAX_RESPONSE_BYTES,
        cancellation,
    )
    .map_err(|error| error.into_vox_error("doubao.settings_http_failed"))?;
    ensure_not_cancelled(cancellation)?;
    parse_settings_token(&response.body).map_err(|_| {
        bootstrap_error(
            ErrorCategory::Protocol,
            "doubao.invalid_settings_response",
            "Doubao settings response did not contain a valid ASR token",
        )
    })
}

fn validate_bootstrap_config(
    config: &BootstrapHttpConfig,
    context: &BootstrapRequestContext,
) -> Result<(), VoxError> {
    if !(1..=300).contains(&config.timeout_seconds) {
        return Err(bootstrap_error(
            ErrorCategory::Configuration,
            "doubao.invalid_timeout",
            "Doubao bootstrap timeout must be between 1 and 300 seconds",
        ));
    }
    for endpoint in [&config.registration_endpoint, &config.settings_endpoint] {
        voxtype_provider_common::validate_endpoint(
            endpoint,
            "Doubao bootstrap endpoints must use HTTPS or loopback HTTP",
        )?;
        if endpoint.contains(['?', '#']) || endpoint.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(bootstrap_error(
                ErrorCategory::Configuration,
                "doubao.invalid_bootstrap_endpoint",
                "Doubao bootstrap endpoint must not contain a query, fragment, or control byte",
            ));
        }
    }
    if context.user_agent.is_empty()
        || context.user_agent.len() > 512
        || context
            .user_agent
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(bootstrap_error(
            ErrorCategory::Configuration,
            "doubao.invalid_user_agent",
            "Doubao client User-Agent is invalid",
        ));
    }
    if context.common_query.len() > MAX_QUERY_PARAMETERS {
        return Err(bootstrap_error(
            ErrorCategory::Configuration,
            "doubao.too_many_query_parameters",
            "Doubao client metadata contains too many query parameters",
        ));
    }
    for (key, value) in &context.common_query {
        if !valid_query_key(key)
            || value.len() > 1024
            || value.bytes().any(|byte| byte.is_ascii_control())
            || key == "device_id"
        {
            return Err(bootstrap_error(
                ErrorCategory::Configuration,
                "doubao.invalid_query_metadata",
                "Doubao client query metadata is invalid or overrides a reserved identity field",
            ));
        }
    }
    Ok(())
}

fn validate_registered_device(registered: &RegisteredDevice) -> Result<(), VoxError> {
    if registered.device_id.is_empty()
        || registered.device_id.len() > 128
        || registered
            .device_id
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        return Err(bootstrap_error(
            ErrorCategory::Configuration,
            "doubao.invalid_registered_device",
            "registered Doubao device identifier is invalid",
        ));
    }
    Ok(())
}

fn bootstrap_url(
    endpoint: &str,
    common_query: &[(String, String)],
    identity_query: &[(&str, &str)],
) -> Result<String, VoxError> {
    let mut url = String::with_capacity(endpoint.len() + 256);
    url.push_str(endpoint);
    let mut first = true;
    for (key, value) in common_query
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .chain(identity_query.iter().copied())
    {
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(&percent_encode(key));
        url.push('=');
        url.push_str(&percent_encode(value));
        if url.len() > 16 * 1024 {
            return Err(bootstrap_error(
                ErrorCategory::Configuration,
                "doubao.query_too_large",
                "Doubao bootstrap query is too large",
            ));
        }
    }
    Ok(url)
}

fn valid_query_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn curl_post_config(
    url: &str,
    user_agent: &str,
    content_type: &str,
    body: &[u8],
    extra_header: Option<(&str, &str)>,
) -> String {
    let body = String::from_utf8_lossy(body);
    let mut config = format!(
        "request = \"POST\"\nurl = \"{}\"\nheader = \"User-Agent: {}\"\nheader = \"Accept: application/json\"\nheader = \"Content-Type: {}\"\ndata = \"{}\"\n",
        escape_curl_config(url),
        escape_curl_config(user_agent),
        escape_curl_config(content_type),
        escape_curl_config(&body),
    );
    if let Some((name, value)) = extra_header {
        use std::fmt::Write as _;
        writeln!(
            &mut config,
            "header = \"{}: {}\"",
            escape_curl_config(name),
            escape_curl_config(value)
        )
        .expect("writing to a String cannot fail");
    }
    config
}

fn bootstrap_error(category: ErrorCategory, code: &'static str, message: &'static str) -> VoxError {
    VoxError::new(category, code, message)
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), VoxError> {
    if cancellation.is_cancelled() {
        Err(bootstrap_error(
            ErrorCategory::Cancelled,
            "doubao.bootstrap_cancelled",
            "Doubao credential bootstrap was cancelled",
        ))
    } else {
        Ok(())
    }
}

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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{Receiver, sync_channel};
    use std::thread;
    use std::time::Duration;

    fn loopback_response(path: &str, body: &'static str) -> (String, Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let address = listener.local_addr().expect("loopback address");
        let (sender, receiver) = sync_channel(1);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept curl request");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("set request timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).expect("read curl request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            sender.send(request).expect("retain request fixture");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write loopback response");
        });
        (format!("http://{address}{path}"), receiver)
    }

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
    fn bootstrap_http_keeps_identity_out_of_debug_and_builds_exact_requests() {
        let (registration_endpoint, registration_request) = loopback_response(
            "/register",
            r#"{"data":{"device_id":123456,"install_id":"654321"}}"#,
        );
        let (settings_endpoint, settings_request) = loopback_response(
            "/settings",
            r#"{"data":{"settings":{"asr_config":{"app_key":"temporary-token"}}}}"#,
        );
        let config = BootstrapHttpConfig {
            registration_endpoint,
            settings_endpoint,
            timeout_seconds: 3,
        };
        let context = BootstrapRequestContext {
            user_agent: "VoxType fixture/1".to_owned(),
            common_query: vec![
                ("aid".to_owned(), "fixture".to_owned()),
                ("locale".to_owned(), "zh CN".to_owned()),
                ("cdid".to_owned(), "cdid-secret".to_owned()),
            ],
        };
        let identity = DeviceIdentity {
            client_udid: "client-secret".to_owned(),
            open_udid: "open-secret".to_owned(),
            cdid: "cdid-secret".to_owned(),
        };
        let document = RegistrationDocument::from_json(&serde_json::json!({
            "magic_tag": "fixture",
            "header": {"model": "fixture-device"},
            "generated_at": 1
        }))
        .expect("registration document");
        assert!(!format!("{context:?}").contains("fixture/1"));
        assert!(!format!("{identity:?}").contains("client-secret"));
        assert!(!format!("{document:?}").contains("magic_tag"));

        let registered =
            register_device_http(&config, &context, &document, &CancellationToken::new())
                .expect("device registration");
        let token =
            fetch_settings_token_http(&config, &context, &registered, &CancellationToken::new())
                .expect("settings token");
        assert_eq!(token.expose(), "temporary-token");

        let registration = String::from_utf8(
            registration_request
                .recv_timeout(Duration::from_secs(3))
                .expect("registration request"),
        )
        .expect("registration request UTF-8");
        assert!(
            registration
                .starts_with("POST /register?aid=fixture&locale=zh%20CN&cdid=cdid-secret HTTP/1.1")
        );
        assert!(!registration.contains("client-secret"));
        assert!(!registration.contains("open-secret"));
        assert!(registration.contains("User-Agent: VoxType fixture/1"));
        assert!(registration.contains("\r\n\r\n{\"generated_at\":1,"));

        let settings = String::from_utf8(
            settings_request
                .recv_timeout(Duration::from_secs(3))
                .expect("settings request"),
        )
        .expect("settings request UTF-8");
        assert!(settings.starts_with(
            "POST /settings?aid=fixture&locale=zh%20CN&cdid=cdid-secret&device_id=123456 HTTP/1.1"
        ));
        assert!(settings.contains("x-ss-stub: 46C03B52742B3F2615A3ABDF1636B754"));
        assert!(settings.ends_with("\r\n\r\nbody=null"));
    }

    #[test]
    fn bootstrap_http_rejects_reserved_metadata_and_pre_cancelled_work() {
        let config = BootstrapHttpConfig {
            registration_endpoint: "http://127.0.0.1:1/register".to_owned(),
            settings_endpoint: "http://127.0.0.1:1/settings".to_owned(),
            timeout_seconds: 1,
        };
        let mut context = BootstrapRequestContext {
            user_agent: "fixture".to_owned(),
            common_query: vec![("device_id".to_owned(), "override".to_owned())],
        };
        let registered = RegisteredDevice {
            device_id: "123".to_owned(),
            install_id: String::new(),
        };
        let error =
            fetch_settings_token_http(&config, &context, &registered, &CancellationToken::new())
                .expect_err("reserved metadata must fail");
        assert_eq!(error.code(), "doubao.invalid_query_metadata");

        context.common_query.clear();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = fetch_settings_token_http(&config, &context, &registered, &cancellation)
            .expect_err("pre-cancelled bootstrap must fail");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(!error.to_string().contains("123"));
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
