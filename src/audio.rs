//! Audio capture adapter using the `PipeWire` `PulseAudio` compatibility service.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct Recording {
    child: Child,
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_millis: u64,
}

impl Recording {
    /// Starts mono 16 kHz signed 16-bit capture from the default source.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the runtime directory/file cannot be created or
    /// `parec` cannot be started.
    pub fn start() -> io::Result<Self> {
        let path = recording_path()?;
        let mut child = Command::new("parec")
            .args([
                "--raw",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--device=@DEFAULT_SOURCE@",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        thread::sleep(Duration::from_millis(50));
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "parec exited during startup with {status}"
            )));
        }

        Ok(Self { child, path })
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
        let bytes = fs::metadata(&self.path)?.len();
        let duration_millis = bytes.saturating_mul(1_000) / 32_000;
        Ok(RecordingResult {
            path: self.path.clone(),
            bytes,
            duration_millis,
        })
    }

    /// Terminates capture and removes the partial recording.
    pub fn cancel(mut self) {
        let _result = self.child.kill();
        let _result = self.child.wait();
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

#[cfg(test)]
mod tests {
    #[test]
    fn duration_uses_sixteen_kilohertz_mono_i16() {
        let bytes = 32_000_u64;
        assert_eq!(bytes.saturating_mul(1_000) / 32_000, 1_000);
    }
}
