//! Dependency-free domain contracts for `VoxType`.

pub mod audio;
pub mod cancellation;
pub mod error;
pub mod provider;
pub mod routing;
pub mod session;

pub use audio::{AudioChunk, AudioFormat, SampleFormat};
pub use cancellation::CancellationToken;
pub use error::{ErrorCategory, VoxError};
pub use provider::{
    AudioAcceptance, ProviderAttemptFailure, ProviderCapabilities, ProviderId, RecognitionEvent,
    RecognitionRequest,
};
pub use routing::{
    FallbackReason, ProviderHealth, ProviderRouter, ReplayPolicy, RoutePlan, RoutingPolicy,
};
pub use session::{
    Command, CommandEffect, SessionId, SessionMachine, SessionState, StartRequest, TriggerMode,
};
