//! KDE/Wayland text insertion through the clipboard and an authorized paste chord.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertionResult {
    pub clipboard_restored: bool,
    pub backend: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct ClipboardInserter {
    restore_delay: Duration,
    restore_clipboard: bool,
}

#[derive(Debug)]
enum ClipboardSnapshot {
    Empty,
    Text(Vec<u8>),
    Unsupported,
}

impl Default for ClipboardInserter {
    fn default() -> Self {
        Self {
            restore_delay: Duration::from_millis(250),
            restore_clipboard: true,
        }
    }
}

impl ClipboardInserter {
    #[must_use]
    pub const fn with_restore(mut self, restore_clipboard: bool) -> Self {
        self.restore_clipboard = restore_clipboard;
        self
    }

    /// Inserts Unicode text using `wl-copy` and the existing user `ydotoold`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if clipboard ownership or the paste chord fails.
    pub fn insert(&self, text: &str) -> io::Result<InsertionResult> {
        if text.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to insert empty text",
            ));
        }

        let previous = if self.restore_clipboard {
            let snapshot = read_clipboard_snapshot()?;
            if matches!(snapshot, ClipboardSnapshot::Unsupported) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "clipboard contains non-text data that VoxType cannot restore safely",
                ));
            }
            Some(snapshot)
        } else {
            None
        };
        write_clipboard(text.as_bytes())?;
        send_paste_chord()?;
        thread::sleep(self.restore_delay);

        let clipboard_restored = if self.restore_clipboard && clipboard_matches(text.as_bytes()) {
            match previous {
                Some(ClipboardSnapshot::Text(contents)) => write_clipboard(&contents).is_ok(),
                Some(ClipboardSnapshot::Empty) => clear_clipboard(),
                Some(ClipboardSnapshot::Unsupported) | None => false,
            }
        } else {
            false
        };

        Ok(InsertionResult {
            clipboard_restored,
            backend: "wl-copy+ydotool",
        })
    }

    /// Copies Unicode text without synthesizing keyboard input.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if clipboard ownership cannot be acquired.
    pub fn copy(&self, text: &str) -> io::Result<InsertionResult> {
        if text.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to copy empty text",
            ));
        }
        write_clipboard(text.as_bytes())?;
        Ok(InsertionResult {
            clipboard_restored: false,
            backend: "copy-only",
        })
    }
}

fn read_clipboard() -> Option<Vec<u8>> {
    Command::new("wl-paste")
        .args(["--no-newline", "--type", "text"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn read_clipboard_snapshot() -> io::Result<ClipboardSnapshot> {
    let text = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text"])
        .output()?;
    if text.status.success() {
        return Ok(ClipboardSnapshot::Text(text.stdout));
    }

    let types = Command::new("wl-paste").arg("--list-types").output()?;
    if types.status.success() && !types.stdout.is_empty() {
        Ok(ClipboardSnapshot::Unsupported)
    } else {
        Ok(ClipboardSnapshot::Empty)
    }
}

fn clipboard_matches(expected: &[u8]) -> bool {
    let current = read_clipboard();
    clipboard_contents_match(current.as_deref(), expected)
}

fn clipboard_contents_match(current: Option<&[u8]>, expected: &[u8]) -> bool {
    current.is_some_and(|current| current == expected)
}

fn clear_clipboard() -> bool {
    Command::new("wl-copy")
        .arg("--clear")
        .status()
        .is_ok_and(|status| status.success())
}

fn write_clipboard(contents: &[u8]) -> io::Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("wl-copy stdin is unavailable"))?
        .write_all(contents)?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("wl-copy failed with {status}")))
    }
}

fn send_paste_chord() -> io::Result<()> {
    // Linux input-event codes: KEY_LEFTCTRL=29 and KEY_V=47.
    let status = Command::new("ydotool")
        .args(["key", "29:1", "47:1", "47:0", "29:0"])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("ydotool failed with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_restore_delay_allows_paste_consumer_to_read() {
        assert!(ClipboardInserter::default().restore_delay >= Duration::from_millis(100));
    }

    #[test]
    fn restore_requires_voxtype_to_still_own_clipboard() {
        assert!(clipboard_contents_match(Some(b"dictation"), b"dictation"));
        assert!(!clipboard_contents_match(Some(b"user copy"), b"dictation"));
        assert!(!clipboard_contents_match(None, b"dictation"));
    }
}
