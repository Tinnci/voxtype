//! Audio capture adapter using native `pw-record` with a `parec` fallback.

use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct Recording {
    child: Child,
    path: PathBuf,
    backend: &'static str,
    reader: Option<JoinHandle<io::Result<u64>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_millis: u64,
    pub backend: &'static str,
}

impl Recording {
    /// Starts mono 16 kHz signed 16-bit capture from the default source.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the runtime directory/file cannot be created or
    /// no `PipeWire` capture command can be started.
    pub fn start() -> io::Result<Self> {
        let path = recording_path()?;
        let mut output = File::create(&path)?;
        let (backend, mut command) = capture_command();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut source = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("parec stdout is unavailable"))?;
        let reader = thread::Builder::new()
            .name("voxtype-audio-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                let mut written = 0_u64;
                loop {
                    let count = source.read(&mut buffer)?;
                    if count == 0 {
                        output.flush()?;
                        return Ok(written);
                    }
                    output.write_all(&buffer[..count])?;
                    output.flush()?;
                    written = written.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
                }
            })?;

        thread::sleep(Duration::from_millis(50));
        if let Some(status) = child.try_wait()? {
            let _reader_result = reader.join();
            return Err(io::Error::other(format!(
                "{backend} exited during startup with {status}"
            )));
        }

        Ok(Self {
            child,
            path,
            backend,
            reader: Some(reader),
        })
    }

    /// Stops capture and returns raw PCM metadata.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the child cannot be terminated or the recorded
    /// file cannot be inspected.
    pub fn stop(mut self) -> io::Result<RecordingResult> {
        if self.child.try_wait()?.is_none() {
            let status = Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()?;
            if !status.success() {
                self.child.kill()?;
            }
        }
        let _status = self.child.wait()?;
        self.join_reader()?;
        let bytes = fs::metadata(&self.path)?.len();
        let duration_millis = bytes.saturating_mul(1_000) / 32_000;
        Ok(RecordingResult {
            path: self.path.clone(),
            bytes,
            duration_millis,
            backend: self.backend,
        })
    }

    /// Terminates capture and removes the partial recording.
    pub fn cancel(mut self) {
        let _result = self.child.kill();
        let _result = self.child.wait();
        let _result = self.join_reader();
        let _result = fs::remove_file(&self.path);
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _result = self.child.kill();
            let _result = self.child.wait();
        }
        let _result = self.join_reader();
    }
}

impl Recording {
    fn join_reader(&mut self) -> io::Result<()> {
        let Some(reader) = self.reader.take() else {
            return Ok(());
        };
        reader
            .join()
            .map_err(|_| io::Error::other("audio reader thread panicked"))??;
        Ok(())
    }
}

fn recording_path() -> io::Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let directory = runtime.join("voxtype");
    fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok(directory.join(format!("recording-{}-{timestamp}.pcm", std::process::id())))
}

fn capture_command() -> (&'static str, Command) {
    if command_exists("pw-record") {
        return ("pw-record", {
            let mut command = Command::new("pw-record");
            command.args([
                "--raw",
                "--format=s16",
                "--rate=16000",
                "--channels=1",
                "--media-category=Capture",
                "--media-role=Communication",
                "--latency=20ms",
                "-",
            ]);
            command
        });
    }
    let mut command = Command::new("parec");
    command.args([
        "--raw",
        "--format=s16le",
        "--rate=16000",
        "--channels=1",
        "--device=@DEFAULT_SOURCE@",
        "--latency-msec=20",
        "--process-time-msec=20",
    ]);
    ("parec", command)
}

fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(command).is_file())
}

/// Removes abandoned `VoxType` PCM captures from the current runtime directory.
///
/// Files outside the `recording-*.pcm` namespace are never touched.
pub fn cleanup_stale_recordings() {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    cleanup_directory(&runtime.join("voxtype"));
}

fn cleanup_directory(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("recording-") && name.ends_with(".pcm") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_uses_sixteen_kilohertz_mono_i16() {
        let bytes = 32_000_u64;
        assert_eq!(bytes.saturating_mul(1_000) / 32_000, 1_000);
    }

    #[test]
    fn prefers_native_pipewire_capture_when_available() {
        let (backend, _command) = capture_command();
        if command_exists("pw-record") {
            assert_eq!(backend, "pw-record");
        } else {
            assert_eq!(backend, "parec");
        }
    }

    #[test]
    fn stale_cleanup_only_removes_recordings() {
        let directory = std::env::temp_dir().join(format!(
            "voxtype-cleanup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        fs::write(directory.join("recording-old.pcm"), b"audio").expect("write recording");
        fs::write(directory.join("fcitx.sock"), b"sentinel").expect("write sentinel");

        cleanup_directory(&directory);

        assert!(!directory.join("recording-old.pcm").exists());
        assert!(directory.join("fcitx.sock").exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
