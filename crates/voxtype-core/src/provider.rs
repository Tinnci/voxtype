//! Provider-neutral ASR types and attempt lifecycle evidence.

use crate::{AudioFormat, VoxError};
use std::fmt::{self, Display, Formatter};

/// What is known about a provider receiving the current audio.
///
/// Failure after a transport starts is often ambiguous: a client may be
/// cancelled or lose the connection after sending some bytes but before the
/// provider acknowledges them. Routing must treat that case conservatively so
/// audio is not replayed to another provider without consent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioAcceptance {
    #[default]
    NotAccepted,
    PossiblyAccepted,
    Accepted,
}

impl AudioAcceptance {
    #[must_use]
    pub const fn may_have_left_process(self) -> bool {
        !matches!(self, Self::NotAccepted)
    }
}

/// A provider failure together with the lifecycle evidence needed by routing
/// and usage accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAttemptFailure {
    pub error: VoxError,
    pub transport_started: bool,
    pub audio_acceptance: AudioAcceptance,
}

impl ProviderAttemptFailure {
    #[must_use]
    pub const fn before_transport(error: VoxError) -> Self {
        Self {
            error,
            transport_started: false,
            audio_acceptance: AudioAcceptance::NotAccepted,
        }
    }

    #[must_use]
    pub const fn after_transport(error: VoxError, audio_acceptance: AudioAcceptance) -> Self {
        Self {
            error,
            transport_started: true,
            audio_acceptance,
        }
    }

    #[must_use]
    pub fn into_error(self) -> VoxError {
        self.error
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates a normalized provider identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, too long, or contains anything
    /// other than lowercase ASCII letters, digits, and hyphens.
    pub fn new(value: impl Into<String>) -> Result<Self, VoxError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(Self(value))
        } else {
            Err(VoxError::new(
                crate::ErrorCategory::InvalidArgument,
                "provider.invalid_id",
                "provider ID must contain lowercase ASCII letters, digits, or hyphens",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ProviderId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    pub languages: Vec<String>,
    pub accepted_formats: Vec<AudioFormat>,
    pub streaming: bool,
    pub partial_results: bool,
    pub provider_vad: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognitionRequest {
    pub language: String,
    pub punctuation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecognitionEvent {
    Ready,
    SpeechStarted,
    Partial { text: String, sequence: Option<u64> },
    Final { text: String, sequence: Option<u64> },
    SpeechEnded,
    Finished,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_provider_ids() {
        assert!(ProviderId::new("doubao-unofficial").is_ok());
        assert!(ProviderId::new("bad_provider").is_err());
        assert!(ProviderId::new("").is_err());
    }
}
