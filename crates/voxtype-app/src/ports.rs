//! Runtime-neutral ports implemented by capture and desktop adapters.

use std::fmt::Debug;
use std::path::PathBuf;
use voxtype_core::{SessionId, VoxError};

/// Content-free metrics for one captured PCM frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureFrameMetrics {
    pub frame: u64,
    pub rms: u16,
    pub peak: u16,
    pub clipped_samples: u16,
    pub samples: u16,
}

/// Completed recording passed to VAD and provider orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedAudio {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_millis: u64,
    pub backend: &'static str,
}

/// One active capture owned by the single-session application flow.
pub trait CaptureSession: Debug + Send + Sync {
    /// Stops capture and transfers ownership of its completed PCM spool.
    ///
    /// # Errors
    ///
    /// Returns a normalized capture/IO failure.
    fn stop(self: Box<Self>) -> Result<CapturedAudio, VoxError>;

    /// Cancels capture and removes partial audio.
    fn cancel(self: Box<Self>);

    /// Drains currently available content-free frame metrics.
    fn drain_metrics(&mut self) -> Vec<CaptureFrameMetrics>;
}

/// Starts capture without exposing a concrete PipeWire/process type to the
/// application controller.
pub trait CaptureAdapter: Debug + Send + Sync {
    /// Starts one capture from the configured device or system default.
    ///
    /// # Errors
    ///
    /// Returns a normalized startup/permission/device error.
    fn start(&self, device: Option<&str>) -> Result<Box<dyn CaptureSession>, VoxError>;
}

/// User-configured insertion mode without desktop-specific target data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertionMode {
    Auto,
    Fcitx,
    Clipboard,
    Copy,
}

/// Opaque-enough insertion lease retained for one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertionArm {
    pub session: SessionId,
    pub backend: &'static str,
}

/// Transcript-free result of a completed desktop insertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertionOutcome {
    pub backend: &'static str,
    pub clipboard_restored: bool,
}

/// Focus-safe insertion lifecycle implemented at the desktop boundary.
pub trait InsertionAdapter: Debug + Send + Sync {
    /// Arms the intended target before microphone capture begins.
    ///
    /// # Errors
    ///
    /// Returns a focus, secure-field, permission, or transport failure.
    fn arm(&self, mode: InsertionMode, session: &SessionId) -> Result<InsertionArm, VoxError>;

    /// Commits final text through the previously armed path.
    ///
    /// # Errors
    ///
    /// Returns a focus-generation, dispatch, clipboard, or injection failure.
    fn commit(&self, arm: &InsertionArm, text: &str) -> Result<InsertionOutcome, VoxError>;

    /// Cancels any desktop-side target retained for this session.
    fn cancel(&self, arm: &InsertionArm);

    /// Exercises the explicit compatibility inserter for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns a clipboard/injection failure.
    fn insert_diagnostic(&self, text: &str) -> Result<InsertionOutcome, VoxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_arm_contains_no_transcript_or_target_identity() {
        let arm = InsertionArm {
            session: SessionId::from_counter(7),
            backend: "fcitx",
        };
        assert_eq!(arm.session.as_str(), "session-7");
        assert_eq!(arm.backend, "fcitx");
    }
}
