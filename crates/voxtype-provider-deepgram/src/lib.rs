//! Deepgram prerecorded speech recognition through the system `curl` transport.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use voxtype_core::{ErrorCategory, VoxError};
pub use voxtype_provider_common::SecretString;
use voxtype_provider_common::escape_curl_config;

#[derive(Debug)]
pub struct DeepgramConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: SecretString,
    pub timeout_seconds: u64,
    pub smart_format: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transcription {
    pub text: String,
}

/// Transcribes raw mono 16 kHz signed 16-bit PCM with Deepgram's official
/// prerecorded API.
///
/// # Errors
///
/// Returns a normalized error if WAV staging, `curl`, the HTTP service, or the
/// Deepgram response is invalid.
pub fn transcribe_pcm(
    config: &DeepgramConfig,
    pcm_path: &Path,
    language: &str,
) -> Result<Transcription, VoxError> {
    validate_endpoint(&config.endpoint)?;
    let wav_path =
        voxtype_provider_common::pcm_to_wav(pcm_path, "deepgram.wav").map_err(io_error)?;
    let result = request(config, &wav_path, language);
    let _remove_result = fs::remove_file(wav_path);
    result
}

fn request(
    config: &DeepgramConfig,
    wav_path: &Path,
    language: &str,
) -> Result<Transcription, VoxError> {
    let url = request_url(config, language);
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
            "--header",
            "Content-Type: application/octet-stream",
            "--data-binary",
            &format!("@{}", wav_path.display()),
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        VoxError::new(
            ErrorCategory::Unavailable,
            "provider.deepgram.curl_unavailable",
            format!("could not start curl: {error}"),
        )
    })?;
    let config_input = format!(
        "header = \"Authorization: Token {}\"\nheader = \"Accept: application/json\"\n",
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
        let message = sanitize_transport_error(&String::from_utf8_lossy(&output.stderr));
        let (category, retryable) = classify_http_failure(&message);
        return Err(
            VoxError::new(category, "provider.deepgram.http_failed", message)
                .with_retryable(retryable),
        );
    }

    parse_transcription(&output.stdout)
}

fn parse_transcription(response: &[u8]) -> Result<Transcription, VoxError> {
    let value: serde_json::Value = serde_json::from_slice(response).map_err(|error| {
        VoxError::new(
            ErrorCategory::Protocol,
            "provider.deepgram.invalid_json",
            format!("Deepgram returned invalid JSON: {error}"),
        )
    })?;
    let text = value
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.deepgram.missing_transcript",
                "Deepgram response did not contain a non-empty transcript",
            )
        })?;
    Ok(Transcription {
        text: text.to_owned(),
    })
}

fn request_url(config: &DeepgramConfig, language: &str) -> String {
    let separator = if config.endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut url = format!(
        "{}{separator}model={}&smart_format={}",
        config.endpoint,
        encode_query(&config.model),
        config.smart_format
    );
    if !language.is_empty() {
        url.push_str("&language=");
        url.push_str(&encode_query(language));
    }
    url
}

fn encode_query(value: &str) -> String {
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

fn classify_http_failure(message: &str) -> (ErrorCategory, bool) {
    if message.contains("401") || message.contains("403") {
        (ErrorCategory::Authentication, false)
    } else if message.contains("400") || message.contains("404") || message.contains("422") {
        (ErrorCategory::Protocol, false)
    } else if message.contains("429") {
        (ErrorCategory::RateLimited, true)
    } else {
        (ErrorCategory::Unavailable, true)
    }
}

/// Validates that an endpoint uses HTTPS, except for loopback integration tests.
///
/// # Errors
///
/// Returns a configuration error for remote plain HTTP or unsupported schemes.
pub fn validate_endpoint(endpoint: &str) -> Result<(), VoxError> {
    voxtype_provider_common::validate_endpoint(
        endpoint,
        "Deepgram endpoint must use HTTPS or loopback HTTP",
    )
}

fn sanitize_transport_error(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("Deepgram request failed");
    let truncated = first_line.chars().take(300).collect::<String>();
    if truncated.is_empty() {
        "Deepgram request failed".to_owned()
    } else {
        truncated
    }
}

fn io_error(error: io::Error) -> VoxError {
    let message = error.to_string();
    drop(error);
    VoxError::new(
        ErrorCategory::Internal,
        "provider.deepgram.io_failed",
        message,
    )
}

fn internal(message: &'static str) -> VoxError {
    VoxError::new(
        ErrorCategory::Internal,
        "provider.deepgram.internal",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn builds_encoded_request_url() {
        let config = fixture_config("http://127.0.0.1:8080/v1/listen?detect_language=false");
        assert_eq!(
            request_url(&config, "zh-CN mixed"),
            "http://127.0.0.1:8080/v1/listen?detect_language=false&model=nova-3&smart_format=true&language=zh-CN%20mixed"
        );
    }

    #[test]
    fn rejects_plain_remote_http() {
        assert!(validate_endpoint("http://api.deepgram.com/v1/listen").is_err());
        assert!(validate_endpoint("https://api.deepgram.com/v1/listen").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080/v1/listen").is_ok());
    }

    #[test]
    fn redacts_secret_debug_output() {
        let secret = SecretString::new("sensitive".to_owned());
        assert_eq!(format!("{secret:?}"), "SecretString([redacted])");
    }

    #[test]
    fn classifies_provider_failures() {
        assert_eq!(
            classify_http_failure("server returned 401"),
            (ErrorCategory::Authentication, false)
        );
        assert_eq!(
            classify_http_failure("server returned 422"),
            (ErrorCategory::Protocol, false)
        );
        assert_eq!(
            classify_http_failure("server returned 429"),
            (ErrorCategory::RateLimited, true)
        );
        assert_eq!(
            classify_http_failure("connection reset"),
            (ErrorCategory::Unavailable, true)
        );
    }

    #[test]
    fn rejects_malformed_or_empty_transcripts() {
        assert!(parse_transcription(b"not-json").is_err());
        assert!(
            parse_transcription(
                br#"{"results":{"channels":[{"alternatives":[{"transcript":"  "}]}]}}"#
            )
            .is_err()
        );
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
            assert!(request_text.contains("Authorization: Token test-key"));
            assert!(request_text.contains("model=nova-3"));
            assert!(request_text.contains("language=zh"));
            assert!(request.windows(4).any(|window| window == b"RIFF"));
            let body = br#"{"results":{"channels":[{"alternatives":[{"transcript":"Deepgram transcript"}]}]}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("response headers");
            stream.write_all(body).expect("response body");
        });

        let directory =
            std::env::temp_dir().join(format!("voxtype-deepgram-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let pcm = directory.join("sample.pcm");
        fs::write(&pcm, vec![0_u8; 3_200]).expect("PCM fixture");
        let result = transcribe_pcm(
            &DeepgramConfig {
                endpoint: format!("http://{address}/v1/listen"),
                ..fixture_config("")
            },
            &pcm,
            "zh",
        )
        .expect("loopback transcription");
        assert_eq!(result.text, "Deepgram transcript");
        server.join().expect("loopback server");
        let _result = fs::remove_dir_all(directory);
    }

    fn fixture_config(endpoint: &str) -> DeepgramConfig {
        DeepgramConfig {
            endpoint: endpoint.to_owned(),
            model: "nova-3".to_owned(),
            api_key: SecretString::new("test-key".to_owned()),
            timeout_seconds: 5,
            smart_format: true,
        }
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
