//! Focus-safe desktop insertion adapter.

use crate::{desktop::ClipboardInserter, fcitx::FcitxBridge};
use voxtype_app::{InsertionAdapter, InsertionArm, InsertionMode, InsertionOutcome};
use voxtype_core::{ErrorCategory, SessionId, VoxError};

#[derive(Clone, Copy, Debug)]
pub struct DesktopInsertionAdapter {
    clipboard: ClipboardInserter,
}

impl DesktopInsertionAdapter {
    #[must_use]
    pub fn new(restore_clipboard: bool) -> Self {
        Self {
            clipboard: ClipboardInserter::default().with_restore(restore_clipboard),
        }
    }
}

impl InsertionAdapter for DesktopInsertionAdapter {
    fn arm(&self, mode: InsertionMode, session: &SessionId) -> Result<InsertionArm, VoxError> {
        let backend = match mode {
            InsertionMode::Auto => match FcitxBridge.arm(session) {
                Ok(()) => "fcitx5",
                Err(error) if may_auto_fallback_from_fcitx(&error) => "copy-only",
                Err(error) => return Err(error),
            },
            InsertionMode::Fcitx => {
                FcitxBridge.arm(session)?;
                "fcitx5"
            }
            InsertionMode::Clipboard => "wl-copy+ydotool",
            InsertionMode::Copy => "copy-only",
        };
        Ok(InsertionArm {
            session: session.clone(),
            backend,
        })
    }

    fn commit(&self, arm: &InsertionArm, text: &str) -> Result<InsertionOutcome, VoxError> {
        match arm.backend {
            "fcitx5" => {
                FcitxBridge.commit(&arm.session, text)?;
                Ok(InsertionOutcome {
                    backend: "fcitx5",
                    clipboard_restored: true,
                })
            }
            "wl-copy+ydotool" => self
                .clipboard
                .insert(text)
                .map(|result| InsertionOutcome {
                    backend: result.backend,
                    clipboard_restored: result.clipboard_restored,
                })
                .map_err(|error| {
                    VoxError::new(
                        ErrorCategory::Unavailable,
                        "desktop.insertion_failed",
                        error.to_string(),
                    )
                }),
            "copy-only" => self
                .clipboard
                .copy(text)
                .map(|result| InsertionOutcome {
                    backend: result.backend,
                    clipboard_restored: result.clipboard_restored,
                })
                .map_err(|error| {
                    VoxError::new(
                        ErrorCategory::Unavailable,
                        "desktop.copy_failed",
                        error.to_string(),
                    )
                }),
            _ => Err(VoxError::new(
                ErrorCategory::InvalidState,
                "desktop.invalid_arm",
                "insertion arm selected an unknown backend",
            )),
        }
    }

    fn cancel(&self, arm: &InsertionArm) {
        if arm.backend == "fcitx5" {
            FcitxBridge.cancel(&arm.session);
        }
    }

    fn insert_diagnostic(&self, text: &str) -> Result<InsertionOutcome, VoxError> {
        self.clipboard
            .insert(text)
            .map(|result| InsertionOutcome {
                backend: result.backend,
                clipboard_restored: result.clipboard_restored,
            })
            .map_err(|error| {
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "desktop.insertion_failed",
                    error.to_string(),
                )
            })
    }
}

fn may_auto_fallback_from_fcitx(error: &VoxError) -> bool {
    matches!(
        error.code(),
        "fcitx.transport_failed" | "fcitx.runtime_unavailable"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_fallback_never_bypasses_focus_or_security_rejection() {
        let secure = VoxError::new(
            ErrorCategory::Permission,
            "fcitx.bridge_rejected",
            "secure context",
        );
        let missing_focus = VoxError::new(
            ErrorCategory::InvalidState,
            "fcitx.bridge_rejected",
            "no focused context",
        );
        let unavailable = VoxError::new(
            ErrorCategory::Unavailable,
            "fcitx.transport_failed",
            "socket unavailable",
        );
        assert!(!may_auto_fallback_from_fcitx(&secure));
        assert!(!may_auto_fallback_from_fcitx(&missing_focus));
        assert!(may_auto_fallback_from_fcitx(&unavailable));
    }
}
