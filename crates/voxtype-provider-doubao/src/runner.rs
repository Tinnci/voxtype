//! End-to-end PCM, Opus, WebSocket, and protocol session orchestration.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use voxtype_core::{AudioAcceptance, ErrorCategory, ProviderAttemptFailure, VoxError};
use voxtype_provider_common::{CancellationToken, SecretString};

use crate::PCM_FRAME_BYTES;
use crate::opus_codec::{OpusFrameEncoder, SystemOpusEncoder};
use crate::session::{
    DoubaoSessionProtocol, SecretRequest, SessionEvent, SessionOutcome, SessionPhase,
    SessionProtocolError,
};
use crate::websocket::{
    BinaryWebSocket, FlushResult, PollResult, QueueBinaryResult, SocketEvent, WebSocketSpec,
    WebSocketTransportError, connect_websocket,
};

const MAX_PCM_BYTES: u64 = 16_000 * 2 * 3_600;
const MAX_INBOUND_PER_TICK: usize = 8;

#[derive(Clone, Eq, PartialEq)]
pub struct DoubaoRunConfig {
    pub websocket: WebSocketSpec,
    pub request_id: String,
    pub session_json: Vec<u8>,
    pub phase_timeout: Duration,
    pub total_timeout: Duration,
    pub frame_interval: Duration,
}

impl std::fmt::Debug for DoubaoRunConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DoubaoRunConfig")
            .field("websocket", &self.websocket)
            .field("request_id", &"[redacted]")
            .field(
                "session_json",
                &format_args!("[{} redacted bytes]", self.session_json.len()),
            )
            .field("phase_timeout", &self.phase_timeout)
            .field("total_timeout", &self.total_timeout)
            .field("frame_interval", &self.frame_interval)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoubaoTranscription {
    pub text: String,
    pub sent_audio_millis: u64,
    pub partial_results: u64,
    pub provider_vad_started: bool,
    pub provider_vad_finished: bool,
}

/// Runs a real system-libopus recognition attempt.
///
/// # Errors
///
/// Returns structured lifecycle evidence for configuration, PCM, codec,
/// connection, timeout, cancellation, provider, or protocol failures.
pub fn transcribe_pcm_with_evidence(
    config: &DoubaoRunConfig,
    token: &SecretString,
    pcm_path: &Path,
    cancellation: &CancellationToken,
) -> Result<DoubaoTranscription, ProviderAttemptFailure> {
    let encoder = SystemOpusEncoder::new().map_err(|_| {
        ProviderAttemptFailure::before_transport(vox_error(
            ErrorCategory::Unavailable,
            "doubao.opus_unavailable",
            "System Opus encoder is unavailable",
            false,
        ))
    })?;
    transcribe_pcm_with_encoder(config, token, pcm_path, cancellation, encoder)
}

/// Runs one attempt with a replaceable encoder for deterministic transport
/// tests.
///
/// # Errors
///
/// Returns the same structured failures as [`transcribe_pcm_with_evidence`].
pub fn transcribe_pcm_with_encoder<E: OpusFrameEncoder>(
    config: &DoubaoRunConfig,
    token: &SecretString,
    pcm_path: &Path,
    cancellation: &CancellationToken,
    encoder: E,
) -> Result<DoubaoTranscription, ProviderAttemptFailure> {
    validate_run_config(config).map_err(ProviderAttemptFailure::before_transport)?;
    let file = prepare_pcm(pcm_path).map_err(ProviderAttemptFailure::before_transport)?;
    let protocol = DoubaoSessionProtocol::new(config.request_id.clone())
        .map_err(protocol_vox_error)
        .map_err(ProviderAttemptFailure::before_transport)?;
    let socket = connect_websocket(&config.websocket, cancellation).map_err(|error| {
        let vox = error.into_vox_error();
        if error.category() == ErrorCategory::Configuration {
            ProviderAttemptFailure::before_transport(vox)
        } else {
            ProviderAttemptFailure::after_transport(vox, AudioAcceptance::NotAccepted)
        }
    })?;
    let total_deadline = Instant::now() + config.total_timeout;
    let mut driver = SessionDriver {
        socket,
        protocol,
        cancellation,
        total_deadline,
        phase_timeout: config.phase_timeout,
        poll_interval: config.websocket.poll_interval,
        partial_results: 0,
        outcome: None,
    };
    let result = driver.run(file, encoder, token, config);
    if result.is_ok() {
        driver.socket.close_now();
    } else {
        driver.protocol.cancel();
        driver.socket.close_now();
    }
    result.map_err(|error| {
        ProviderAttemptFailure::after_transport(error, driver.protocol.audio_acceptance())
    })
}

struct SessionDriver<'a> {
    socket: BinaryWebSocket,
    protocol: DoubaoSessionProtocol,
    cancellation: &'a CancellationToken,
    total_deadline: Instant,
    phase_timeout: Duration,
    poll_interval: Duration,
    partial_results: u64,
    outcome: Option<SessionOutcome>,
}

impl SessionDriver<'_> {
    fn run<E: OpusFrameEncoder>(
        &mut self,
        file: File,
        mut encoder: E,
        token: &SecretString,
        config: &DoubaoRunConfig,
    ) -> Result<DoubaoTranscription, VoxError> {
        let task_request = self
            .protocol
            .start_task(token)
            .map_err(protocol_vox_error)?;
        self.send_secret(&task_request, self.phase_deadline())?;
        self.wait_for_phase(SessionPhase::TaskStarted, self.phase_deadline())?;

        let session_request = self
            .protocol
            .start_session(token, &config.session_json)
            .map_err(protocol_vox_error)?;
        self.send_secret(&session_request, self.phase_deadline())?;
        self.wait_for_phase(SessionPhase::Streaming, self.phase_deadline())?;

        let session_started_at = Instant::now();
        let timestamp_base = unix_millis()?;
        let mut reader = BufReader::with_capacity(PCM_FRAME_BYTES * 2, file);
        while let Some(frame) = read_pcm_frame(&mut reader)? {
            self.wait_for_send_slot(
                session_started_at
                    + config.frame_interval.saturating_mul(
                        u32::try_from(self.protocol.sent_audio_frames()).unwrap_or(u32::MAX),
                    ),
            )?;
            let packet = encoder.encode_20ms(&frame).map_err(|_| {
                vox_error(
                    ErrorCategory::Internal,
                    "doubao.opus_encode_failed",
                    "Could not encode a Doubao Opus frame",
                    false,
                )
            })?;
            let timestamp =
                timestamp_base.saturating_add(self.protocol.sent_audio_frames().saturating_mul(20));
            let request = self
                .protocol
                .audio_request(timestamp, &packet)
                .map_err(protocol_vox_error)?;
            self.send_audio(request, self.phase_deadline())?;
            self.drain_inbound()?;
        }

        let finish_timestamp =
            timestamp_base.saturating_add(self.protocol.sent_audio_frames().saturating_mul(20));
        let marker = self
            .protocol
            .finish_audio(finish_timestamp)
            .map_err(protocol_vox_error)?;
        self.send_plain(marker, self.phase_deadline())?;
        let finish_request = self
            .protocol
            .finish_session(token)
            .map_err(protocol_vox_error)?;
        self.send_secret(&finish_request, self.phase_deadline())?;
        self.wait_for_phase(SessionPhase::Finished, self.phase_deadline())?;
        let outcome = self.outcome.take().ok_or_else(|| {
            vox_error(
                ErrorCategory::Protocol,
                "doubao.missing_session_outcome",
                "Doubao session finished without a retained outcome",
                false,
            )
        })?;
        Ok(DoubaoTranscription {
            text: outcome.transcript,
            sent_audio_millis: outcome.sent_audio_frames.saturating_mul(20),
            partial_results: self.partial_results,
            provider_vad_started: outcome.provider_vad_started,
            provider_vad_finished: outcome.provider_vad_finished,
        })
    }

    fn send_secret(&mut self, request: &SecretRequest, deadline: Instant) -> Result<(), VoxError> {
        self.send_payload(request.as_bytes().to_vec(), deadline, false)
    }

    fn send_plain(&mut self, payload: Vec<u8>, deadline: Instant) -> Result<(), VoxError> {
        self.send_payload(payload, deadline, false)
    }

    fn send_audio(&mut self, payload: Vec<u8>, deadline: Instant) -> Result<(), VoxError> {
        self.send_payload(payload, deadline, true)
    }

    fn send_payload(
        &mut self,
        mut payload: Vec<u8>,
        deadline: Instant,
        audio: bool,
    ) -> Result<(), VoxError> {
        loop {
            self.check_deadlines(deadline)?;
            match self.socket.queue_binary(payload) {
                Ok(QueueBinaryResult::Queued) => {
                    if audio {
                        self.protocol
                            .mark_audio_write_ambiguous()
                            .map_err(protocol_vox_error)?;
                    }
                    break;
                }
                Ok(QueueBinaryResult::Full(returned)) => {
                    payload = returned;
                    self.flush_once(deadline)?;
                }
                Err(error) => {
                    if audio {
                        self.protocol
                            .mark_audio_write_ambiguous()
                            .map_err(protocol_vox_error)?;
                    }
                    return Err(error.into_vox_error());
                }
            }
        }
        loop {
            self.check_deadlines(deadline)?;
            match self
                .socket
                .flush()
                .map_err(WebSocketTransportError::into_vox_error)?
            {
                FlushResult::Flushed => {
                    if audio {
                        self.protocol
                            .confirm_audio_sent()
                            .map_err(protocol_vox_error)?;
                    }
                    return Ok(());
                }
                FlushResult::Pending => {
                    self.pump_once()?;
                    thread::sleep(self.poll_interval);
                }
                FlushResult::Closed => return Err(connection_closed()),
            }
        }
    }

    fn flush_once(&mut self, deadline: Instant) -> Result<(), VoxError> {
        self.check_deadlines(deadline)?;
        match self
            .socket
            .flush()
            .map_err(WebSocketTransportError::into_vox_error)?
        {
            FlushResult::Flushed | FlushResult::Pending => {
                self.pump_once()?;
                Ok(())
            }
            FlushResult::Closed => Err(connection_closed()),
        }
    }

    fn wait_for_phase(
        &mut self,
        expected: SessionPhase,
        deadline: Instant,
    ) -> Result<(), VoxError> {
        while self.protocol.phase() != expected {
            self.check_deadlines(deadline)?;
            self.pump_once()?;
            thread::sleep(self.poll_interval);
        }
        Ok(())
    }

    fn wait_for_send_slot(&mut self, send_at: Instant) -> Result<(), VoxError> {
        while Instant::now() < send_at {
            self.check_deadlines(self.total_deadline)?;
            self.drain_inbound()?;
            let remaining = send_at.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(self.poll_interval));
        }
        Ok(())
    }

    fn drain_inbound(&mut self) -> Result<(), VoxError> {
        for _ in 0..MAX_INBOUND_PER_TICK {
            if !self.pump_once()? {
                break;
            }
        }
        Ok(())
    }

    fn pump_once(&mut self) -> Result<bool, VoxError> {
        match self
            .socket
            .poll()
            .map_err(WebSocketTransportError::into_vox_error)?
        {
            PollResult::Idle => Ok(false),
            PollResult::Closed | PollResult::Event(SocketEvent::Close) => Err(connection_closed()),
            PollResult::Event(SocketEvent::Binary(message)) => {
                if let Some(event) = self
                    .protocol
                    .handle_binary(&message)
                    .map_err(protocol_vox_error)?
                {
                    match event {
                        SessionEvent::Recognition(recognition) if recognition.interim => {
                            self.partial_results = self.partial_results.saturating_add(1);
                        }
                        SessionEvent::Finished(outcome) => self.outcome = Some(outcome),
                        _ => {}
                    }
                }
                Ok(true)
            }
            PollResult::Event(SocketEvent::Ping(_) | SocketEvent::Pong) => Ok(true),
        }
    }

    fn check_deadlines(&self, phase_deadline: Instant) -> Result<(), VoxError> {
        if self.cancellation.is_cancelled() {
            return Err(vox_error(
                ErrorCategory::Cancelled,
                "doubao.session_cancelled",
                "Doubao recognition session was cancelled",
                false,
            ));
        }
        let now = Instant::now();
        if now >= self.total_deadline {
            return Err(vox_error(
                ErrorCategory::Timeout,
                "doubao.total_timeout",
                "Doubao recognition exceeded its total timeout",
                true,
            ));
        }
        if now >= phase_deadline {
            return Err(vox_error(
                ErrorCategory::Timeout,
                "doubao.phase_timeout",
                "Doubao recognition phase timed out",
                true,
            ));
        }
        Ok(())
    }

    fn phase_deadline(&self) -> Instant {
        (Instant::now() + self.phase_timeout).min(self.total_deadline)
    }
}

fn validate_run_config(config: &DoubaoRunConfig) -> Result<(), VoxError> {
    if !(Duration::from_millis(100)..=Duration::from_secs(60)).contains(&config.phase_timeout)
        || config.total_timeout < config.phase_timeout
        || config.total_timeout > Duration::from_secs(3_600)
        || config.frame_interval > Duration::from_millis(100)
    {
        return Err(vox_error(
            ErrorCategory::Configuration,
            "doubao.invalid_run_timeout",
            "Doubao session timing configuration is invalid",
            false,
        ));
    }
    if config.session_json.len() > 256 * 1024
        || !serde_json::from_slice::<serde_json::Value>(&config.session_json)
            .ok()
            .is_some_and(|value| value.is_object())
    {
        return Err(vox_error(
            ErrorCategory::Configuration,
            "doubao.invalid_session_json",
            "Doubao session configuration must be a bounded JSON object",
            false,
        ));
    }
    Ok(())
}

fn prepare_pcm(path: &Path) -> Result<File, VoxError> {
    let file = File::open(path).map_err(|_| {
        vox_error(
            ErrorCategory::Unavailable,
            "doubao.pcm_open_failed",
            "Could not open the PCM recording",
            true,
        )
    })?;
    let length = file
        .metadata()
        .map_err(|_| {
            vox_error(
                ErrorCategory::Unavailable,
                "doubao.pcm_metadata_failed",
                "Could not inspect the PCM recording",
                true,
            )
        })?
        .len();
    if length == 0 || length > MAX_PCM_BYTES || length % 2 != 0 {
        return Err(vox_error(
            ErrorCategory::InvalidArgument,
            "doubao.invalid_pcm",
            "PCM recording must be non-empty bounded 16-bit audio",
            false,
        ));
    }
    Ok(file)
}

fn read_pcm_frame(reader: &mut impl Read) -> Result<Option<[u8; PCM_FRAME_BYTES]>, VoxError> {
    let mut frame = [0_u8; PCM_FRAME_BYTES];
    let mut filled = 0;
    while filled < frame.len() {
        match reader.read(&mut frame[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                return Err(vox_error(
                    ErrorCategory::Unavailable,
                    "doubao.pcm_read_failed",
                    "Could not read the PCM recording",
                    true,
                ));
            }
        }
    }
    if filled % 2 != 0 {
        return Err(vox_error(
            ErrorCategory::InvalidArgument,
            "doubao.invalid_pcm",
            "PCM recording ended with an incomplete sample",
            false,
        ));
    }
    Ok(Some(frame))
}

fn unix_millis() -> Result<u64, VoxError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            vox_error(
                ErrorCategory::Internal,
                "doubao.system_clock_invalid",
                "System clock is before the Unix epoch",
                false,
            )
        })?
        .as_millis();
    u64::try_from(millis).map_err(|_| {
        vox_error(
            ErrorCategory::Internal,
            "doubao.system_clock_overflow",
            "System clock timestamp is too large",
            false,
        )
    })
}

fn protocol_vox_error(error: SessionProtocolError) -> VoxError {
    vox_error(
        ErrorCategory::Protocol,
        error.code(),
        "Doubao session protocol validation failed",
        false,
    )
}

fn connection_closed() -> VoxError {
    vox_error(
        ErrorCategory::Connection,
        "doubao.websocket_closed_early",
        "Doubao WebSocket closed before recognition completed",
        true,
    )
}

fn vox_error(
    category: ErrorCategory,
    code: &'static str,
    message: &'static str,
    retryable: bool,
) -> VoxError {
    VoxError::new(category, code, message).with_retryable(retryable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::sync_channel;

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    struct FakeEncoder;

    impl OpusFrameEncoder for FakeEncoder {
        fn encode_20ms(
            &mut self,
            pcm: &[u8; PCM_FRAME_BYTES],
        ) -> Result<Vec<u8>, crate::opus_codec::OpusCodecError> {
            Ok(vec![pcm[0], pcm[1], 7])
        }
    }

    fn temp_pcm(bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voxtype-doubao-runner-{}-{}.pcm",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, bytes).expect("write PCM fixture");
        path
    }

    fn write_varint(output: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            output.push(u8::try_from(value & 0x7f).expect("seven bits") | 0x80);
            value >>= 7;
        }
        output.push(u8::try_from(value).expect("last byte"));
    }

    fn write_bytes(output: &mut Vec<u8>, field: u32, value: &[u8]) {
        write_varint(output, u64::from(field) << 3 | 2);
        write_varint(output, u64::try_from(value.len()).expect("fixture length"));
        output.extend_from_slice(value);
    }

    fn response(request_id: &str, kind: &str, result: Option<&str>) -> Vec<u8> {
        let mut output = Vec::new();
        write_bytes(&mut output, 1, request_id.as_bytes());
        write_bytes(&mut output, 2, b"task-loopback");
        write_bytes(&mut output, 3, b"ASR");
        write_bytes(&mut output, 4, kind.as_bytes());
        if let Some(result) = result {
            write_bytes(&mut output, 7, result.as_bytes());
        }
        output
    }

    fn request_method(message: &[u8]) -> String {
        let mut cursor = 0;
        while cursor < message.len() {
            let key = read_varint(message, &mut cursor);
            let field = key >> 3;
            let wire = key & 7;
            if wire == 2 {
                let length = usize::try_from(read_varint(message, &mut cursor)).expect("length");
                let end = cursor + length;
                if field == 5 {
                    return String::from_utf8(message[cursor..end].to_vec()).expect("method UTF-8");
                }
                cursor = end;
            } else if wire == 0 {
                let _ = read_varint(message, &mut cursor);
            } else {
                panic!("unexpected fixture wire type");
            }
        }
        String::new()
    }

    fn read_varint(message: &[u8], cursor: &mut usize) -> u64 {
        let mut value = 0_u64;
        for shift in (0..70).step_by(7) {
            let byte = message[*cursor];
            *cursor += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
        }
        panic!("invalid fixture varint")
    }

    #[test]
    fn loopback_runs_full_duplex_session_with_partial_tail() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind runner loopback");
        let address = listener.local_addr().expect("runner address");
        let (first_audio_sender, first_audio_receiver) = sync_channel(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept runner client");
            let mut socket = tungstenite::accept(stream).expect("runner handshake");
            assert_eq!(
                request_method(&socket.read().expect("StartTask").into_data()),
                "StartTask"
            );
            socket
                .send(tungstenite::Message::Binary(
                    response("request-runner", "TaskStarted", None).into(),
                ))
                .expect("TaskStarted");
            assert_eq!(
                request_method(&socket.read().expect("StartSession").into_data()),
                "StartSession"
            );
            socket
                .send(tungstenite::Message::Binary(
                    response("request-runner", "SessionStarted", None).into(),
                ))
                .expect("SessionStarted");

            let mut task_requests = 0_u64;
            loop {
                let message = socket.read().expect("runner request").into_data();
                match request_method(&message).as_str() {
                    "TaskRequest" => {
                        task_requests += 1;
                        if task_requests == 1 {
                            first_audio_sender.send(()).expect("first audio signal");
                            let partial = r#"{"extra":{"packet_number":1,"vad_start":true},"results":[{"text":"部","is_interim":true}]}"#;
                            socket
                                .send(tungstenite::Message::Binary(
                                    response("request-runner", "Result", Some(partial)).into(),
                                ))
                                .expect("partial result");
                        }
                    }
                    "FinishSession" => break,
                    method => panic!("unexpected method {method}"),
                }
            }
            let final_result = r#"{"extra":{"packet_number":2},"results":[{"text":"完整结果","is_interim":false,"is_vad_finished":true}]}"#;
            socket
                .send(tungstenite::Message::Binary(
                    response("request-runner", "Result", Some(final_result)).into(),
                ))
                .expect("final result");
            socket
                .send(tungstenite::Message::Binary(
                    response("request-runner", "SessionFinished", None).into(),
                ))
                .expect("SessionFinished");
            task_requests
        });

        let mut pcm = vec![1_u8; PCM_FRAME_BYTES * 2 + 100];
        pcm[PCM_FRAME_BYTES] = 2;
        let path = temp_pcm(&pcm);
        let config = DoubaoRunConfig {
            websocket: WebSocketSpec {
                endpoint: format!("ws://{address}/session"),
                headers: vec![("User-Agent".to_owned(), "fixture".to_owned())],
                connect_timeout: Duration::from_secs(2),
                poll_interval: Duration::from_millis(1),
            },
            request_id: "request-runner".to_owned(),
            session_json: br#"{"audio_info":{"sample_rate":16000}}"#.to_vec(),
            phase_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(5),
            frame_interval: Duration::ZERO,
        };
        let token = SecretString::try_new("token-fixture".to_owned()).expect("token");
        let transcription = transcribe_pcm_with_encoder(
            &config,
            &token,
            &path,
            &CancellationToken::new(),
            FakeEncoder,
        )
        .expect("full runner session");
        first_audio_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first audio observed");
        assert_eq!(transcription.text, "完整结果");
        assert_eq!(transcription.sent_audio_millis, 60);
        assert_eq!(transcription.partial_results, 1);
        assert!(transcription.provider_vad_started);
        assert!(transcription.provider_vad_finished);
        assert_eq!(server.join().expect("runner server"), 4);
        fs::remove_file(path).expect("remove PCM fixture");
    }

    #[test]
    fn phase_timeout_retains_not_accepted_evidence() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind timeout server");
        let address = listener.local_addr().expect("timeout address");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept timeout client");
            let mut socket = tungstenite::accept(stream).expect("timeout handshake");
            let _ = socket.read().expect("StartTask");
            thread::sleep(Duration::from_millis(300));
        });
        let path = temp_pcm(&[1_u8; PCM_FRAME_BYTES]);
        let config = DoubaoRunConfig {
            websocket: WebSocketSpec {
                endpoint: format!("ws://{address}/timeout"),
                headers: vec![("User-Agent".to_owned(), "fixture".to_owned())],
                connect_timeout: Duration::from_secs(1),
                poll_interval: Duration::from_millis(2),
            },
            request_id: "request-timeout".to_owned(),
            session_json: br#"{"audio_info":{}}"#.to_vec(),
            phase_timeout: Duration::from_millis(100),
            total_timeout: Duration::from_secs(1),
            frame_interval: Duration::ZERO,
        };
        let token = SecretString::try_new("token-fixture".to_owned()).expect("token");
        let error = transcribe_pcm_with_encoder(
            &config,
            &token,
            &path,
            &CancellationToken::new(),
            FakeEncoder,
        )
        .expect_err("TaskStarted timeout");
        assert_eq!(error.error.category(), ErrorCategory::Timeout);
        assert_eq!(error.audio_acceptance, AudioAcceptance::NotAccepted);
        assert!(error.transport_started);
        server.join().expect("timeout server");
        fs::remove_file(path).expect("remove PCM fixture");
    }
}
