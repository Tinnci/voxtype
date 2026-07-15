//! Qt 6 QML runtime discovery shared by desktop helper binaries.

use std::path::PathBuf;

/// Returns the configured or distribution-standard Qt 6 QML runtime.
#[must_use]
pub fn runtime() -> PathBuf {
    if let Some(path) = std::env::var_os("VOXTYPE_QML_RUNTIME") {
        return PathBuf::from(path);
    }
    for path in ["/usr/lib/qt6/bin/qml6", "/usr/libexec/qt6/qml6"] {
        if std::path::Path::new(path).is_file() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from("qml6")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_has_a_program_name() {
        assert!(!runtime().as_os_str().is_empty());
    }
}
