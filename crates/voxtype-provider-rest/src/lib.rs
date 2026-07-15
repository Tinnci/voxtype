//! OpenAI-compatible batch transcription through the system `curl` transport.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use voxtype_core::{ErrorCategory, VoxError};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

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
    validate_endpoint(&config.endpoint)?;
    let wav_path = pcm_to_wav(pcm_path).map_err(io_error)?;
    let result = request(config, &wav_path, language);
    let _remove_result = fs::remove_file(wav_path);
    result
}

fn request(
    config: &RestProviderConfig,
    wav_path: &Path,
    language: &str,
) -> Result<Transcription, VoxError> {
    let mut command = Command::new("curl");
    command
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--location",
            "--request",
            "POST",
            "--max-time",
            &config.timeout_seconds.to_string(),
            "--config",
            "-",
            "--form",
            &format!("file=@{};type=audio/wav", wav_path.display()),
            "--form-string",
            &format!("model={}", config.model),
            "--form-string",
            "response_format=json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !language.is_empty() {
        command.args(["--form-string", &format!("language={language}")]);
    }
    command.arg(&config.endpoint);

    let mut child = command.spawn().map_err(|error| {
        VoxError::new(
            ErrorCategory::Unavailable,
            "provider.curl_unavailable",
            format!("could not start curl: {error}"),
        )
    })?;
    let config_input = format!(
        "header = \"Authorization: Bearer {}\"\nheader = \"Accept: application/json\"\n",
        escape_curl_config(config.api_key.expose())
    );
    child
        .stdin
        .take()
        .ok_or_else(|| internal("curl stdin is unavailable"))?
        .write_all(config_input.as_bytes())
        .map_err(io_error)?;
    let output = child.wait_with_output().map_err(io_error)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message_text = sanitize_transport_error(&message);
        let (category, retryable) = classify_http_failure(&message_text);
        return Err(
            VoxError::new(category, "provider.http_failed", message_text).with_retryable(retryable),
        );
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        VoxError::new(
            ErrorCategory::Protocol,
            "provider.invalid_json",
            format!("provider returned invalid JSON: {error}"),
        )
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
        })?;
    Ok(Transcription {
        text: text.to_owned(),
    })
}

fn classify_http_failure(message: &str) -> (ErrorCategory, bool) {
    if message.contains("401") || message.contains("403") {
        (ErrorCategory::Authentication, false)
    } else if message.contains("429") {
        (ErrorCategory::RateLimited, true)
    } else {
        (ErrorCategory::Unavailable, true)
    }
}

/// Validates that a provider endpoint uses HTTPS, except for loopback tests.
///
/// # Errors
///
/// Returns a configuration error for remote plain HTTP or unsupported schemes.
pub fn validate_endpoint(endpoint: &str) -> Result<(), VoxError> {
    if endpoint.starts_with("https://") || is_loopback_http(endpoint) {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::Configuration,
            "provider.insecure_endpoint",
            "provider endpoint must use HTTPS or loopback HTTP",
        ))
    }
}

fn is_loopback_http(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

fn pcm_to_wav(pcm_path: &Path) -> io::Result<PathBuf> {
    let pcm = fs::read(pcm_path)?;
    let data_length = u32::try_from(pcm.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "recording is too large"))?;
    let riff_length = data_length
        .checked_add(36)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAV size overflow"))?;
    let wav_path = pcm_path.with_extension("wav");
    let mut file = File::create(&wav_path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_length.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&16_000_u32.to_le_bytes())?;
    file.write_all(&32_000_u32.to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&16_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_length.to_le_bytes())?;
    file.write_all(&pcm)?;
    Ok(wav_path)
}

fn escape_curl_config(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sanitize_transport_error(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("HTTP request failed");
    let truncated = first_line.chars().take(300).collect::<String>();
    if truncated.is_empty() {
        "HTTP request failed".to_owned()
    } else {
        truncated
    }
}

fn io_error(error: io::Error) -> VoxError {
    let message = error.to_string();
    drop(error);
    VoxError::new(ErrorCategory::Internal, "provider.io_failed", message)
}

fn internal(message: &'static str) -> VoxError {
    VoxError::new(ErrorCategory::Internal, "provider.internal", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_http_failures_for_routing() {
        assert_eq!(
            classify_http_failure("curl: server returned error: 401"),
            (ErrorCategory::Authentication, false)
        );
        assert_eq!(
            classify_http_failure("curl: server returned error: 429"),
            (ErrorCategory::RateLimited, true)
        );
        assert_eq!(
            classify_http_failure("curl: connection reset"),
            (ErrorCategory::Unavailable, true)
        );
    }
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

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
    fn redacts_secret_debug_output() {
        let secret = SecretString::new("sensitive".to_owned());
        assert_eq!(format!("{secret:?}"), "SecretString([redacted])");
    }

    #[test]
    fn transcribes_against_loopback_http() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
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
            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.contains("Authorization: Bearer test-key"));
            assert!(request_text.contains("test-model"));
            assert!(request.windows(4).any(|window| window == b"RIFF"));
            let body = br#"{"text":"loopback transcript"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("response headers");
            stream.write_all(body).expect("response body");
        });

        let directory =
            std::env::temp_dir().join(format!("voxtype-rest-loopback-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let pcm = directory.join("sample.pcm");
        fs::write(&pcm, vec![0_u8; 3_200]).expect("PCM fixture");
        let result = transcribe_pcm(
            &RestProviderConfig {
                endpoint: format!("http://{address}/v1/audio/transcriptions"),
                model: "test-model".to_owned(),
                api_key: SecretString::new("test-key".to_owned()),
                timeout_seconds: 5,
            },
            &pcm,
            "zh",
        )
        .expect("loopback transcription");
        assert_eq!(result.text, "loopback transcript");
        server.join().expect("loopback server");
        let _result = fs::remove_dir_all(directory);
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
