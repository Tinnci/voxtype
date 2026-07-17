//! Dependency-free domain contracts for `VoxType`.

pub mod audio;
pub mod error;
pub mod provider;
pub mod routing;
pub mod session;

pub use audio::{AudioChunk, AudioFormat, SampleFormat};
pub use error::{ErrorCategory, VoxError};
pub use provider::{
    AsrProvider, AudioAcceptance, AudioSink, ProviderAttemptFailure, ProviderCapabilities,
    ProviderConnection, ProviderId, RecognitionControl, RecognitionEvent, RecognitionEvents,
    RecognitionRequest,
};
pub use routing::{
    FallbackReason, ProviderHealth, ProviderRouter, ReplayPolicy, RoutePlan, RoutingPolicy,
};
pub use session::{
    Command, CommandEffect, SessionId, SessionMachine, SessionState, StartRequest, TriggerMode,
};
