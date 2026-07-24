//! Small audited helpers shared by cloud provider protocol adapters.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use voxtype_core::{AudioAcceptance, ErrorCategory, ProviderAttemptFailure, VoxError};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub use voxtype_core::CancellationToken;

/// Maximum provider response retained in memory.
///
/// ASR transcript JSON is normally only a few kilobytes. A one MiB ceiling
/// leaves ample room for metadata while preventing a broken endpoint from
/// growing the daemon without bound.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 16 * 1024;
const HTTP_STATUS_MARKER: &str = "VOXTYPE_HTTP_STATUS:";
const UPLOAD_BYTES_MARKER: &str = "VOXTYPE_UPLOAD_BYTES:";
const CURL_WRITE_OUT: &str =
    "%{stderr}\nVOXTYPE_HTTP_STATUS:%{http_code}\nVOXTYPE_UPLOAD_BYTES:%{size_upload}\n";

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Creates a secret after rejecting control characters that could escape a
    /// transport configuration line.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for empty values or ASCII control
    /// characters.
    pub fn try_new(value: String) -> Result<Self, VoxError> {
        validate_secret_bytes(value.as_bytes())?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Validates secret material accepted by CLI and settings-store boundaries.
///
/// # Errors
///
/// Returns an authentication error for empty values or ASCII control bytes.
pub fn validate_secret_bytes(value: &[u8]) -> Result<(), VoxError> {
    if value.is_empty() || value.iter().any(u8::is_ascii_control) {
        return Err(VoxError::new(
            ErrorCategory::Authentication,
            "secret.invalid_characters",
            "secret must be non-empty and contain no control characters",
        ));
    }
    Ok(())
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

/// Successful output from one non-redirecting curl request.
#[derive(Debug)]
pub struct CurlResponse {
    pub http_status: u16,
    pub body: Vec<u8>,
}

/// A normalized curl or HTTP failure that can be mapped to a provider-specific
/// stable error code without losing routing semantics.
#[derive(Debug)]
pub struct CurlFailure {
    category: ErrorCategory,
    retryable: bool,
    message: String,
    http_status: Option<u16>,
    curl_exit_code: Option<i32>,
    transport_started: bool,
    audio_acceptance: AudioAcceptance,
}

impl CurlFailure {
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }

    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    #[must_use]
    pub const fn curl_exit_code(&self) -> Option<i32> {
        self.curl_exit_code
    }

    #[must_use]
    pub const fn transport_started(&self) -> bool {
        self.transport_started
    }

    #[must_use]
    pub const fn audio_acceptance(&self) -> AudioAcceptance {
        self.audio_acceptance
    }

    /// Converts the transport failure into the application's stable error
    /// representation while preserving category and retryability.
    #[must_use]
    pub fn into_vox_error(self, code: &'static str) -> VoxError {
        VoxError::new(self.category, code, self.message).with_retryable(self.retryable)
    }

    /// Converts the transport failure while retaining replay and usage
    /// lifecycle evidence.
    #[must_use]
    pub fn into_attempt_failure(self, code: &'static str) -> ProviderAttemptFailure {
        let transport_started = self.transport_started;
        let audio_acceptance = self.audio_acceptance;
        let error = self.into_vox_error(code);
        ProviderAttemptFailure {
            error,
            transport_started,
            audio_acceptance,
        }
    }

    fn with_transport_evidence(mut self, audio_acceptance: AudioAcceptance) -> Self {
        self.transport_started = true;
        self.audio_acceptance = audio_acceptance;
        self
    }
}

/// Executes one bounded HTTP request with the system curl binary.
///
/// Common security and failure behavior is enforced here:
///
/// - user curl configuration is disabled;
/// - only HTTP(S) schemes are permitted at the transport layer;
/// - redirects are not followed;
/// - the authorization configuration is written through private stdin;
/// - response and diagnostic memory are bounded;
/// - HTTP status and curl exit status are classified independently.
///
/// Provider adapters supply only protocol-specific arguments such as headers,
/// multipart fields, request bodies, and the final `--url` value.
///
/// # Errors
///
/// Returns a normalized failure for process startup, timeout, connection,
/// oversized response, non-success HTTP status, or local pipe errors.
pub fn execute_curl<I, S>(
    args: I,
    timeout_seconds: u64,
    private_config: &[u8],
    max_response_bytes: usize,
) -> Result<CurlResponse, CurlFailure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    execute_curl_cancellable(
        args,
        timeout_seconds,
        private_config,
        max_response_bytes,
        &CancellationToken::new(),
    )
}

/// Executes one bounded curl request that can be interrupted by another thread.
///
/// # Errors
///
/// Returns [`ErrorCategory::Cancelled`] after terminating and reaping curl when
/// the token is cancelled, or the same transport failures as [`execute_curl`].
pub fn execute_curl_cancellable<I, S>(
    args: I,
    timeout_seconds: u64,
    private_config: &[u8],
    max_response_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<CurlResponse, CurlFailure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if cancellation.is_cancelled() {
        return Err(cancelled_failure());
    }
    let max_response_bytes = max_response_bytes.max(1);
    let mut command = curl_command(timeout_seconds, max_response_bytes);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        failure(
            ErrorCategory::Unavailable,
            true,
            format!("could not start curl: {error}"),
            None,
            None,
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| {
            failure(
                ErrorCategory::Internal,
                false,
                "curl stdout is unavailable".to_owned(),
                None,
                None,
            )
        })
        .map_err(|error| error.with_transport_evidence(AudioAcceptance::PossiblyAccepted))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| {
            failure(
                ErrorCategory::Internal,
                false,
                "curl stderr is unavailable".to_owned(),
                None,
                None,
            )
        })
        .map_err(|error| error.with_transport_evidence(AudioAcceptance::PossiblyAccepted))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, max_response_bytes));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));

    let stdin_error = child.stdin.take().map_or_else(
        || Some(io::Error::other("curl stdin is unavailable")),
        |mut stdin| stdin.write_all(private_config).err(),
    );
    let (status, was_cancelled) = wait_for_curl(&mut child, cancellation)
        .map_err(|error| error.with_transport_evidence(AudioAcceptance::PossiblyAccepted))?;
    let body = join_reader(stdout_reader, "stdout")
        .map_err(|error| error.with_transport_evidence(AudioAcceptance::PossiblyAccepted))?;
    let diagnostics = join_reader(stderr_reader, "stderr")
        .map_err(|error| error.with_transport_evidence(AudioAcceptance::PossiblyAccepted))?;
    finish_curl_attempt(
        status,
        was_cancelled,
        body,
        &diagnostics,
        stdin_error,
        max_response_bytes,
    )
}

fn finish_curl_attempt(
    status: std::process::ExitStatus,
    was_cancelled: bool,
    body: BoundedRead,
    diagnostics: &BoundedRead,
    stdin_error: Option<io::Error>,
    max_response_bytes: usize,
) -> Result<CurlResponse, CurlFailure> {
    let curl_exit_code = status.code();
    let http_status = parse_http_status(&diagnostics.bytes);
    let audio_acceptance = classify_audio_acceptance(
        parse_upload_bytes(&diagnostics.bytes),
        curl_exit_code,
        http_status,
        was_cancelled,
    );
    if was_cancelled {
        return Err(cancelled_failure().with_transport_evidence(audio_acceptance));
    }
    let diagnostic = sanitized_diagnostic(&diagnostics.bytes);

    if body.overflowed {
        return Err(failure(
            ErrorCategory::Protocol,
            false,
            format!("provider response exceeded {max_response_bytes} bytes"),
            http_status,
            curl_exit_code,
        )
        .with_transport_evidence(audio_acceptance));
    }

    if let Some(error) = stdin_error {
        return Err(failure(
            ErrorCategory::Connection,
            true,
            format!("could not provide curl request credentials: {error}"),
            http_status,
            curl_exit_code,
        )
        .with_transport_evidence(audio_acceptance));
    }

    if !status.success() {
        return Err(classify_curl_failure(
            curl_exit_code,
            http_status,
            diagnostic.as_deref(),
            max_response_bytes,
        )
        .with_transport_evidence(audio_acceptance));
    }

    let Some(http_status) = http_status else {
        return Err(failure(
            ErrorCategory::Protocol,
            false,
            "curl completed without an HTTP status".to_owned(),
            None,
            curl_exit_code,
        )
        .with_transport_evidence(audio_acceptance));
    };
    if !(200..300).contains(&http_status) {
        return Err(
            classify_http_failure(http_status, curl_exit_code, diagnostic.as_deref())
                .with_transport_evidence(audio_acceptance),
        );
    }

    Ok(CurlResponse {
        http_status,
        body: body.bytes,
    })
}

fn wait_for_curl(
    child: &mut std::process::Child,
    cancellation: &CancellationToken,
) -> Result<(std::process::ExitStatus, bool), CurlFailure> {
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            failure(
                ErrorCategory::Unavailable,
                true,
                format!("could not wait for curl: {error}"),
                None,
                None,
            )
        })? {
            return Ok((status, false));
        }
        if cancellation.is_cancelled() {
            let _ = child.kill();
            let status = child.wait().map_err(|error| {
                failure(
                    ErrorCategory::Unavailable,
                    true,
                    format!("could not reap cancelled curl: {error}"),
                    None,
                    None,
                )
            })?;
            return Ok((status, true));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn cancelled_failure() -> CurlFailure {
    failure(
        ErrorCategory::Cancelled,
        false,
        "provider request was cancelled".to_owned(),
        None,
        None,
    )
}

fn curl_command(timeout_seconds: u64, max_response_bytes: usize) -> Command {
    let mut command = Command::new("curl");
    command
        // --disable must be the first curl argument to suppress ~/.curlrc.
        .arg("--disable")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--proto",
            "=http,https",
            "--max-redirs",
            "0",
            "--max-time",
            &timeout_seconds.to_string(),
            "--max-filesize",
            &max_response_bytes.to_string(),
            "--write-out",
            CURL_WRITE_OUT,
            "--config",
            "-",
        ]);
    command
}

#[derive(Debug)]
struct BoundedRead {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut overflowed = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        overflowed |= retained < count;
    }
    Ok(BoundedRead { bytes, overflowed })
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<BoundedRead>>,
    stream: &'static str,
) -> Result<BoundedRead, CurlFailure> {
    reader
        .join()
        .map_err(|_| {
            failure(
                ErrorCategory::Internal,
                false,
                format!("curl {stream} reader panicked"),
                None,
                None,
            )
        })?
        .map_err(|error| {
            failure(
                ErrorCategory::Internal,
                false,
                format!("could not read curl {stream}: {error}"),
                None,
                None,
            )
        })
}

fn parse_http_status(stderr: &[u8]) -> Option<u16> {
    let diagnostics = String::from_utf8_lossy(stderr);
    diagnostics.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix(HTTP_STATUS_MARKER)
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|status| *status != 0)
    })
}

fn parse_upload_bytes(stderr: &[u8]) -> Option<u64> {
    let diagnostics = String::from_utf8_lossy(stderr);
    diagnostics.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix(UPLOAD_BYTES_MARKER)
            .and_then(|value| value.split('.').next())
            .and_then(|value| value.parse::<u64>().ok())
    })
}

fn classify_audio_acceptance(
    uploaded_bytes: Option<u64>,
    curl_exit_code: Option<i32>,
    http_status: Option<u16>,
    was_cancelled: bool,
) -> AudioAcceptance {
    if uploaded_bytes.is_some_and(|bytes| bytes > 0) {
        return if curl_exit_code == Some(0)
            && http_status.is_some_and(|status| !(300..400).contains(&status))
        {
            AudioAcceptance::Accepted
        } else {
            AudioAcceptance::PossiblyAccepted
        };
    }
    if uploaded_bytes == Some(0)
        || (!was_cancelled && matches!(curl_exit_code, Some(5 | 6 | 7 | 35 | 60)))
    {
        AudioAcceptance::NotAccepted
    } else {
        AudioAcceptance::PossiblyAccepted
    }
}

fn sanitized_diagnostic(stderr: &[u8]) -> Option<String> {
    let diagnostics = String::from_utf8_lossy(stderr);
    diagnostics
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with(HTTP_STATUS_MARKER)
                && !line.starts_with(UPLOAD_BYTES_MARKER)
        })
        .map(|line| line.chars().take(300).collect())
}

fn classify_curl_failure(
    curl_exit_code: Option<i32>,
    http_status: Option<u16>,
    diagnostic: Option<&str>,
    max_response_bytes: usize,
) -> CurlFailure {
    match curl_exit_code {
        Some(22) if http_status.is_some() => classify_http_failure(
            http_status.expect("status checked above"),
            curl_exit_code,
            diagnostic,
        ),
        Some(28) => failure(
            ErrorCategory::Timeout,
            true,
            diagnostic
                .unwrap_or("provider request timed out")
                .to_owned(),
            http_status,
            curl_exit_code,
        ),
        Some(63) => failure(
            ErrorCategory::Protocol,
            false,
            format!("provider response exceeded {max_response_bytes} bytes"),
            http_status,
            curl_exit_code,
        ),
        Some(5 | 6 | 7 | 35 | 52 | 55 | 56) => failure(
            ErrorCategory::Connection,
            true,
            diagnostic
                .unwrap_or("provider connection failed")
                .to_owned(),
            http_status,
            curl_exit_code,
        ),
        Some(60) => failure(
            ErrorCategory::Connection,
            false,
            diagnostic
                .unwrap_or("provider TLS certificate validation failed")
                .to_owned(),
            http_status,
            curl_exit_code,
        ),
        _ if http_status.is_some_and(|status| !(200..300).contains(&status)) => {
            classify_http_failure(
                http_status.expect("status checked above"),
                curl_exit_code,
                diagnostic,
            )
        }
        _ => failure(
            ErrorCategory::Unavailable,
            true,
            diagnostic.unwrap_or("provider transport failed").to_owned(),
            http_status,
            curl_exit_code,
        ),
    }
}

fn classify_http_failure(
    http_status: u16,
    curl_exit_code: Option<i32>,
    diagnostic: Option<&str>,
) -> CurlFailure {
    let (category, retryable, default_message) = match http_status {
        401 | 403 | 407 => (
            ErrorCategory::Authentication,
            false,
            "provider rejected authentication",
        ),
        408 => (ErrorCategory::Timeout, true, "provider request timed out"),
        429 => (
            ErrorCategory::RateLimited,
            true,
            "provider rate limit was reached",
        ),
        500..=599 => (
            ErrorCategory::Unavailable,
            true,
            "provider service is unavailable",
        ),
        300..=399 => (
            ErrorCategory::Protocol,
            false,
            "provider returned a redirect; redirects are disabled",
        ),
        _ => (
            ErrorCategory::Protocol,
            false,
            "provider rejected the request",
        ),
    };
    let detail = diagnostic.unwrap_or(default_message);
    failure(
        category,
        retryable,
        format!("HTTP {http_status}: {detail}"),
        Some(http_status),
        curl_exit_code,
    )
}

fn failure(
    category: ErrorCategory,
    retryable: bool,
    message: String,
    http_status: Option<u16>,
    curl_exit_code: Option<i32>,
) -> CurlFailure {
    CurlFailure {
        category,
        retryable,
        message,
        http_status,
        curl_exit_code,
        transport_started: false,
        audio_acceptance: AudioAcceptance::NotAccepted,
    }
}

/// Validates HTTPS endpoints while allowing loopback HTTP for offline tests.
///
/// # Errors
///
/// Returns a configuration error for remote plain HTTP or unsupported schemes.
pub fn validate_endpoint(endpoint: &str, message: &'static str) -> Result<(), VoxError> {
    if endpoint.starts_with("https://") || is_loopback_http(endpoint) {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::Configuration,
            "provider.insecure_endpoint",
            message,
        ))
    }
}

/// Converts `VoxType`'s mono 16 kHz signed 16-bit PCM recording to a WAV file.
///
/// # Errors
///
/// Returns an I/O error for unreadable input, oversized audio, or staging-file
/// failures.
pub fn pcm_to_wav(pcm_path: &Path, extension: &str) -> io::Result<PathBuf> {
    let pcm = fs::read(pcm_path)?;
    let data_length = u32::try_from(pcm.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "recording is too large"))?;
    let riff_length = data_length
        .checked_add(36)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAV size overflow"))?;
    let wav_path = pcm_path.with_extension(extension);
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

#[must_use]
pub fn escape_curl_config(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_https_and_loopback_only() {
        assert!(validate_endpoint("https://example.com/asr", "secure").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:8080/asr", "secure").is_ok());
        assert!(validate_endpoint("http://localhost/asr", "secure").is_ok());
        assert!(validate_endpoint("http://[::1]:8080/asr", "secure").is_ok());
        assert!(validate_endpoint("http://127.0.0.1.example/asr", "secure").is_err());
        assert!(validate_endpoint("http://example.com/asr", "secure").is_err());
    }

    #[test]
    fn writes_valid_wav_header() {
        let directory =
            std::env::temp_dir().join(format!("voxtype-common-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let pcm = directory.join("sample.pcm");
        fs::write(&pcm, vec![0_u8; 640]).expect("PCM fixture");
        let wav = pcm_to_wav(&pcm, "wav").expect("WAV conversion");
        let bytes = fs::read(&wav).expect("WAV fixture");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(bytes.len(), 684);
        let _result = fs::remove_dir_all(directory);
    }

    #[test]
    fn redacts_secret_debug_output() {
        let secret = SecretString::new("sensitive".to_owned());
        assert_eq!(format!("{secret:?}"), "SecretString([redacted])");
    }

    #[test]
    fn rejects_control_characters_in_secrets() {
        assert!(SecretString::try_new("valid-key".to_owned()).is_ok());
        assert!(SecretString::try_new("line-one\nnext-directive".to_owned()).is_err());
        assert!(SecretString::try_new(String::new()).is_err());
    }

    #[test]
    fn pre_cancelled_transport_never_starts_a_request() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = execute_curl_cancellable(
            ["--url", "http://127.0.0.1:1/unreachable"],
            1,
            &[],
            1024,
            &cancellation,
        )
        .expect_err("pre-cancelled transport must fail");
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(!error.is_retryable());
        assert!(!error.transport_started());
        assert_eq!(error.audio_acceptance(), AudioAcceptance::NotAccepted);
    }

    #[test]
    fn upload_evidence_is_conservative() {
        assert_eq!(
            parse_upload_bytes(b"detail\nVOXTYPE_UPLOAD_BYTES:684\n"),
            Some(684)
        );
        assert_eq!(
            classify_audio_acceptance(Some(684), Some(0), Some(200), false),
            AudioAcceptance::Accepted
        );
        assert_eq!(
            classify_audio_acceptance(Some(12), Some(56), None, false),
            AudioAcceptance::PossiblyAccepted
        );
        assert_eq!(
            classify_audio_acceptance(Some(0), Some(7), None, false),
            AudioAcceptance::NotAccepted
        );
        assert_eq!(
            classify_audio_acceptance(None, None, None, true),
            AudioAcceptance::PossiblyAccepted
        );
        assert_eq!(
            classify_audio_acceptance(Some(684), Some(0), Some(307), false),
            AudioAcceptance::PossiblyAccepted
        );
        assert_eq!(
            classify_audio_acceptance(Some(684), Some(0), None, false),
            AudioAcceptance::PossiblyAccepted
        );
    }
}
