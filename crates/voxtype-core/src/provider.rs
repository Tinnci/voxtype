//! ASR provider contracts.

use crate::{AudioChunk, AudioFormat, VoxError};
use std::fmt::{self, Display, Formatter};

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

pub trait AudioSink: Send {
    /// Sends one provider-neutral audio chunk.
    ///
    /// # Errors
    ///
    /// Returns a normalized provider error if the chunk cannot be accepted.
    fn send(&mut self, chunk: &AudioChunk) -> Result<(), VoxError>;

    /// Marks the end of audio input.
    ///
    /// # Errors
    ///
    /// Returns a normalized provider error if finalization cannot be requested.
    fn finish(&mut self) -> Result<(), VoxError>;
}

pub trait RecognitionEvents: Send {
    /// Waits for the next normalized recognition event.
    ///
    /// # Errors
    ///
    /// Returns a normalized provider error on timeout, cancellation, transport,
    /// or protocol failure.
    fn next(&mut self) -> Result<RecognitionEvent, VoxError>;
}

pub trait RecognitionControl: Send + Sync {
    /// Cancels the provider session.
    ///
    /// # Errors
    ///
    /// Returns a normalized provider error if cancellation cannot be delivered.
    fn cancel(&self) -> Result<(), VoxError>;
}

pub struct ProviderConnection {
    pub audio: Box<dyn AudioSink>,
    pub events: Box<dyn RecognitionEvents>,
    pub control: Box<dyn RecognitionControl>,
}

pub trait AsrProvider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn capabilities(&self) -> ProviderCapabilities;

    /// Opens a recognition session.
    ///
    /// # Errors
    ///
    /// Returns a normalized configuration, credential, transport, or provider
    /// availability error.
    fn connect(&self, request: &RecognitionRequest) -> Result<ProviderConnection, VoxError>;
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
