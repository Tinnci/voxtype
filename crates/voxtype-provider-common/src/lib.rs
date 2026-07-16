//! Small audited helpers shared by cloud provider protocol adapters.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use voxtype_core::{ErrorCategory, VoxError};
use zeroize::{Zeroize, ZeroizeOnDrop};

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
}
