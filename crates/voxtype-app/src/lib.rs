//! Runtime-neutral application orchestration for `VoxType`.
//!
//! This crate is the application boundary between the dependency-free domain
//! model and concrete D-Bus, desktop, capture, secret-store, and provider
//! adapters. It intentionally uses standard-library synchronization only.

mod controller;
mod ports;
mod provider;
mod wake;

pub use controller::{
    AppController, AppEvent, ProviderHealthSnapshot, ProviderUsageSnapshot, TerminalSessionResult,
};
pub use ports::{
    CaptureAdapter, CaptureFrameMetrics, CaptureSession, CapturedAudio, InsertionAdapter,
    InsertionArm, InsertionMode, InsertionOutcome,
};
pub use provider::{
    ProviderAdapter, ProviderRegistry, ProviderSuccess, ProviderTranscript, ProviderUsage,
    RecognitionInput, RecognitionRouteResult, RecognitionSuccess, RouteAttempt,
    run_recognition_route,
};
pub use wake::{WakeHandle, wake_channel};
