//! OpenAI-compatible batch transcription through the system `curl` transport.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use voxtype_core::{AudioAcceptance, ErrorCategory, ProviderAttemptFailure, VoxError};
pub use voxtype_provider_common::{CancellationToken, SecretString};
use voxtype_provider_common::{
    DEFAULT_MAX_RESPONSE_BYTES, escape_curl_config, execute_curl_cancellable,
};

#[derive(Debug)]
pub struct RestProviderConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: SecretString,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transcription {
    pub text: String,
    pub usage: ApiUsage,
}

/// Token counters explicitly reported by a provider response.
///
/// Missing fields remain `None`; `VoxType` does not estimate tokens from audio
/// duration or transcript length.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApiUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Transcribes raw 16 kHz mono signed 16-bit PCM.
///
/// # Errors
///
/// Returns a normalized error if the WAV staging file, `curl` transport, HTTP
/// service, or JSON response is invalid.
pub fn transcribe_pcm(
    config: &RestProviderConfig,
    pcm_path: &Path,
    language: &str,
) -> Result<Transcription, VoxError> {
    transcribe_pcm_cancellable(config, pcm_path, language, &CancellationToken::new())
}

/// Transcribes PCM while allowing another thread to terminate the HTTP request.
///
/// # Errors
///
/// Returns the same errors as [`transcribe_pcm`], including a cancelled error
/// when the supplied token is cancelled.
pub fn transcribe_pcm_cancellable(
    config: &RestProviderConfig,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<Transcription, VoxError> {
    transcribe_pcm_with_evidence(config, pcm_path, language, cancellation)
        .map_err(ProviderAttemptFailure::into_error)
}

/// Transcribes PCM and preserves transport/audio lifecycle evidence on error.
///
/// # Errors
///
/// Returns a structured failure suitable for replay policy and usage
/// accounting.
pub fn transcribe_pcm_with_evidence(
    config: &RestProviderConfig,
    pcm_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<Transcription, ProviderAttemptFailure> {
    validate_endpoint(&config.endpoint).map_err(ProviderAttemptFailure::before_transport)?;
    let wav_path = pcm_to_wav(pcm_path)
        .map_err(|error| ProviderAttemptFailure::before_transport(io_error(error)))?;
    let result = request(config, &wav_path, language, cancellation);
    let _remove_result = fs::remove_file(wav_path);
    result
}

fn request(
    config: &RestProviderConfig,
    wav_path: &Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<Transcription, ProviderAttemptFailure> {
    let mut args = vec![
        "--request".to_owned(),
        "POST".to_owned(),
        "--form".to_owned(),
        format!("file=@{};type=audio/wav", wav_path.display()),
        "--form-string".to_owned(),
        format!("model={}", config.model),
        "--form-string".to_owned(),
        "response_format=json".to_owned(),
    ];
    if !language.is_empty() {
        args.extend(["--form-string".to_owned(), format!("language={language}")]);
    }
    args.extend(["--url".to_owned(), config.endpoint.clone()]);

    let config_input = format!(
        "header = \"Authorization: Bearer {}\"\nheader = \"Accept: application/json\"\n",
        escape_curl_config(config.api_key.expose())
    );
    let response = execute_curl_cancellable(
        &args,
        config.timeout_seconds,
        config_input.as_bytes(),
        DEFAULT_MAX_RESPONSE_BYTES,
        cancellation,
    )
    .map_err(|error| error.into_attempt_failure("provider.http_failed"))?;

    let value: serde_json::Value = serde_json::from_slice(&response.body)
        .map_err(|error| {
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.invalid_json",
                format!("provider returned invalid JSON: {error}"),
            )
        })
        .map_err(|error| {
            ProviderAttemptFailure::after_transport(error, AudioAcceptance::Accepted)
        })?;
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.missing_text",
                "provider response did not contain non-empty text",
            )
        })
        .map_err(|error| {
            ProviderAttemptFailure::after_transport(error, AudioAcceptance::Accepted)
        })?;
    Ok(Transcription {
        text: text.to_owned(),
        usage: parse_usage(&value),
    })
}

fn parse_usage(value: &serde_json::Value) -> ApiUsage {
    let Some(usage) = value.get("usage").and_then(serde_json::Value::as_object) else {
        return ApiUsage::default();
    };
    ApiUsage {
        input_tokens: first_u64(usage, &["input_tokens", "prompt_tokens"]),
        output_tokens: first_u64(usage, &["output_tokens", "completion_tokens"]),
        total_tokens: first_u64(usage, &["total_tokens"]),
    }
}

fn first_u64(object: &serde_json::Map<String, serde_json::Value>, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_u64))
}

/// Validates that a provider endpoint uses HTTPS, except for loopback tests.
///
/// # Errors
///
/// Returns a configuration error for remote plain HTTP or unsupported schemes.
pub fn validate_endpoint(endpoint: &str) -> Result<(), VoxError> {
    voxtype_provider_common::validate_endpoint(
        endpoint,
        "provider endpoint must use HTTPS or loopback HTTP",
    )
}

fn pcm_to_wav(pcm_path: &Path) -> io::Result<PathBuf> {
    voxtype_provider_common::pcm_to_wav(pcm_path, "wav")
}

fn io_error(error: io::Error) -> VoxError {
    let message = error.to_string();
    drop(error);
    VoxError::new(ErrorCategory::Internal, "provider.io_failed", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn writes_valid_wav_header() {
        let directory =
            std::env::temp_dir().join(format!("voxtype-rest-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let pcm = directory.join("sample.pcm");
        fs::write(&pcm, vec![0_u8; 640]).expect("PCM fixture");
        let wav = pcm_to_wav(&pcm).expect("WAV conversion");
        let bytes = fs::read(&wav).expect("WAV fixture");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(bytes.len(), 684);
        let _result = fs::remove_dir_all(directory);
    }

    #[test]
    fn rejects_plain_remote_http() {
        assert!(validate_endpoint("http://example.com/asr").is_err());
        assert!(validate_endpoint("http://127.0.0.1:8080/asr").is_ok());
        assert!(validate_endpoint("http://localhost/asr").is_ok());
        assert!(validate_endpoint("http://[::1]:8080/asr").is_ok());
        assert!(validate_endpoint("http://127.0.0.1.example/asr").is_err());
    }

    #[test]
    fn missing_pcm_fails_before_transport() {
        let error = transcribe_pcm_with_evidence(
            &RestProviderConfig {
                endpoint: "https://example.com/v1/audio/transcriptions".to_owned(),
                model: "test".to_owned(),
                api_key: SecretString::new("test-key".to_owned()),
                timeout_seconds: 5,
            },
            Path::new("/definitely/missing/voxtype-audio.pcm"),
            "zh",
            &CancellationToken::new(),
        )
        .expect_err("missing PCM must fail");
        assert!(!error.transport_started);
        assert_eq!(error.audio_acceptance, AudioAcceptance::NotAccepted);
    }

    #[test]
    fn redacts_secret_debug_output() {
        let secret = SecretString::new("sensitive".to_owned());
        assert_eq!(format!("{secret:?}"), "SecretString([redacted])");
    }

    #[test]
    fn transcribes_against_loopback_http() {
        let body = br#"{"text":"loopback transcript","usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15}}"#.to_vec();
        let (address, server) = spawn_response(200, Vec::new(), body, Duration::ZERO);
        let result = transcribe_fixture(address, 5).expect("loopback transcription");
        assert_eq!(result.text, "loopback transcript");
        assert_eq!(
            result.usage,
            ApiUsage {
                input_tokens: Some(11),
                output_tokens: Some(4),
                total_tokens: Some(15),
            }
        );
        let request = server.join().expect("loopback server");
        let request_text = String::from_utf8_lossy(&request);
        assert!(request_text.contains("Authorization: Bearer test-key"));
        assert!(request_text.contains("test-model"));
        assert!(request.windows(4).any(|window| window == b"RIFF"));
    }

    #[test]
    fn classifies_actual_http_failures() {
        for (status, category, retryable) in [
            (401, ErrorCategory::Authentication, false),
            (429, ErrorCategory::RateLimited, true),
            (503, ErrorCategory::Unavailable, true),
        ] {
            let (address, server) = spawn_response(
                status,
                Vec::new(),
                br#"{"error":"sanitized fixture"}"#.to_vec(),
                Duration::ZERO,
            );
            let error = transcribe_fixture(address, 5).expect_err("HTTP failure expected");
            assert_eq!(error.category(), category);
            assert_eq!(error.is_retryable(), retryable);
            assert!(error.to_string().contains(&status.to_string()));
            assert!(!error.to_string().contains("test-key"));
            server.join().expect("loopback server");
        }
    }

    #[test]
    fn classifies_actual_transport_timeout() {
        let (address, server) = spawn_response(
            200,
            Vec::new(),
            br#"{"text":"too late"}"#.to_vec(),
            Duration::from_millis(1_200),
        );
        let error = transcribe_fixture(address, 1).expect_err("request must time out");
        assert_eq!(error.category(), ErrorCategory::Timeout);
        assert!(error.is_retryable());
        assert!(!error.to_string().contains("test-key"));
        server.join().expect("loopback server");
    }

    #[test]
    fn cancels_an_in_flight_request() {
        let (address, server) = spawn_response(
            200,
            Vec::new(),
            br#"{"text":"too late"}"#.to_vec(),
            Duration::from_millis(500),
        );
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let error = transcribe_fixture_cancellable(address, 5, &cancellation)
            .expect_err("request must be cancelled");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(!error.is_retryable());
        assert!(started.elapsed() < Duration::from_millis(400));
        canceller.join().expect("cancellation thread");
        server.join().expect("loopback server");
    }

    #[test]
    fn refuses_redirect_without_contacting_target() {
        let target = TcpListener::bind("127.0.0.1:0").expect("redirect target");
        let location = format!(
            "Location: http://{}/stolen\r\n",
            target.local_addr().expect("target address")
        );
        let (address, server) =
            spawn_response(302, location.into_bytes(), Vec::new(), Duration::ZERO);
        let error = transcribe_fixture(address, 5).expect_err("redirect must be rejected");
        assert_eq!(error.category(), ErrorCategory::Protocol);
        assert!(!error.is_retryable());
        server.join().expect("redirect server");

        target
            .set_nonblocking(true)
            .expect("nonblocking redirect target");
        let target_result = target.accept();
        assert!(matches!(
            target_result,
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn rejects_oversized_response() {
        let body = vec![b'x'; DEFAULT_MAX_RESPONSE_BYTES + 1];
        let (address, server) = spawn_response(200, Vec::new(), body, Duration::ZERO);
        let error = transcribe_fixture(address, 5).expect_err("response must be bounded");
        assert_eq!(error.category(), ErrorCategory::Protocol);
        assert!(!error.is_retryable());
        server.join().expect("loopback server");
    }

    #[test]
    fn leaves_absent_usage_unknown() {
        let value = serde_json::json!({"text": "hello"});
        assert_eq!(parse_usage(&value), ApiUsage::default());
    }

    #[test]
    fn accepts_openai_token_field_names() {
        let value = serde_json::json!({
            "usage": {"input_tokens": 12, "output_tokens": 3, "total_tokens": 15}
        });
        assert_eq!(
            parse_usage(&value),
            ApiUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                total_tokens: Some(15),
            }
        );
    }

    fn transcribe_fixture(
        address: SocketAddr,
        timeout_seconds: u64,
    ) -> Result<Transcription, VoxError> {
        transcribe_fixture_cancellable(address, timeout_seconds, &CancellationToken::new())
    }

    fn transcribe_fixture_cancellable(
        address: SocketAddr,
        timeout_seconds: u64,
        cancellation: &CancellationToken,
    ) -> Result<Transcription, VoxError> {
        assert_curl_available();
        let unique = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "voxtype-rest-loopback-{}-{timestamp}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temporary directory");
        let pcm = directory.join("sample.pcm");
        fs::write(&pcm, vec![0_u8; 3_200]).expect("PCM fixture");
        let result = transcribe_pcm_cancellable(
            &RestProviderConfig {
                endpoint: format!("http://{address}/v1/audio/transcriptions"),
                model: "test-model".to_owned(),
                api_key: SecretString::new("test-key".to_owned()),
                timeout_seconds,
            },
            &pcm,
            "zh",
            cancellation,
        );
        let _ = fs::remove_dir_all(directory);
        result
    }

    fn spawn_response(
        status: u16,
        extra_headers: Vec<u8>,
        body: Vec<u8>,
        delay: Duration,
    ) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let request = read_request(&mut stream);
            thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            let _ = stream.write_all(&extra_headers);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.write_all(&body);
            request
        });
        (address, server)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    request.extend_from_slice(&buffer[..count]);
                    if complete_http_request(&request) {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("request read failed: {error}"),
            }
        }
        request
    }

    fn assert_curl_available() {
        let output = Command::new("curl")
            .arg("--version")
            .output()
            .expect("curl must be installed for transport tests");
        assert!(output.status.success(), "curl --version failed");
    }

    fn complete_http_request(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
        content_length.is_some_and(|length| request.len() >= header_end + 4 + length)
    }
}
