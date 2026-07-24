//! Audio capture adapter using native `pw-record` with a `parec` fallback.

use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Mutex,
    mpsc::{Receiver, sync_channel},
};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use voxtype_app::{CaptureAdapter, CaptureFrameMetrics, CaptureSession, CapturedAudio};
use voxtype_core::{ErrorCategory, VoxError};

#[derive(Debug)]
pub struct Recording {
    child: Child,
    path: PathBuf,
    backend: &'static str,
    reader: Option<JoinHandle<io::Result<u64>>>,
    frames: Mutex<Receiver<AudioFrameMetrics>>,
    preserve_file_on_drop: bool,
}

/// Bounded, content-free metrics for one 20 ms PCM frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFrameMetrics {
    pub frame: u64,
    pub rms: u16,
    pub peak: u16,
    pub clipped_samples: u16,
    pub samples: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_millis: u64,
    pub backend: &'static str,
}

/// Production capture adapter composed into the daemon.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCaptureAdapter;

impl CaptureAdapter for ProcessCaptureAdapter {
    fn start(&self, device: Option<&str>) -> Result<Box<dyn CaptureSession>, VoxError> {
        Recording::start_with_device(device)
            .map(|recording| Box::new(recording) as Box<dyn CaptureSession>)
            .map_err(|error| {
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "audio.start_failed",
                    error.to_string(),
                )
            })
    }
}

impl CaptureSession for Recording {
    fn stop(self: Box<Self>) -> Result<CapturedAudio, VoxError> {
        Recording::stop(*self)
            .map(|result| CapturedAudio {
                path: result.path,
                bytes: result.bytes,
                duration_millis: result.duration_millis,
                backend: result.backend,
            })
            .map_err(|error| {
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "audio.stop_failed",
                    error.to_string(),
                )
            })
    }

    fn cancel(self: Box<Self>) {
        Recording::cancel(*self);
    }

    fn drain_metrics(&mut self) -> Vec<CaptureFrameMetrics> {
        self.drain_frames()
            .into_iter()
            .map(CaptureFrameMetrics::from)
            .collect()
    }
}

impl From<AudioFrameMetrics> for CaptureFrameMetrics {
    fn from(value: AudioFrameMetrics) -> Self {
        Self {
            frame: value.frame,
            rms: value.rms,
            peak: value.peak,
            clipped_samples: value.clipped_samples,
            samples: value.samples,
        }
    }
}

impl Recording {
    /// Starts mono 16 kHz signed 16-bit capture from the default source.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the runtime directory/file cannot be created or
    /// no `PipeWire` capture command can be started.
    pub fn start() -> io::Result<Self> {
        Self::start_with_device(None)
    }

    /// Starts capture from an optional `PipeWire` node name/target.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the runtime recording file or capture process
    /// cannot be created.
    pub fn start_with_device(device: Option<&str>) -> io::Result<Self> {
        let pending_path = RecordingPathGuard::new(recording_path()?);
        let mut output = File::create(pending_path.path())?;
        let (backend, mut command) = capture_command(device);
        let (frame_sender, frame_receiver) = sync_channel(64);
        let mut child = CaptureChildGuard::new(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );

        let mut source = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("parec stdout is unavailable"))?;
        let reader = thread::Builder::new()
            .name("voxtype-audio-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                let mut pending = Vec::with_capacity(BYTES_PER_FRAME);
                let mut frame = 0_u64;
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
                    pending.extend_from_slice(&buffer[..count]);
                    while pending.len() >= BYTES_PER_FRAME {
                        let metrics = frame_metrics(frame, &pending[..BYTES_PER_FRAME]);
                        let _ = frame_sender.try_send(metrics);
                        pending.drain(..BYTES_PER_FRAME);
                        frame = frame.saturating_add(1);
                    }
                }
            })?;

        thread::sleep(Duration::from_millis(50));
        if let Some(status) = child.child_mut().try_wait()? {
            let _reader_result = reader.join();
            return Err(io::Error::other(format!(
                "{backend} exited during startup with {status}"
            )));
        }

        Ok(Self {
            child: child.take(),
            path: pending_path.retain(),
            backend,
            reader: Some(reader),
            frames: Mutex::new(frame_receiver),
            preserve_file_on_drop: false,
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
        self.preserve_file_on_drop = true;
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

    /// Drains currently available frame metrics without waiting for capture.
    #[must_use]
    pub fn drain_frames(&mut self) -> Vec<AudioFrameMetrics> {
        let mut frames = Vec::new();
        let receiver = self
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Ok(frame) = receiver.try_recv() {
            frames.push(frame);
        }
        frames
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _result = self.child.kill();
            let _result = self.child.wait();
        }
        let _result = self.join_reader();
        if !self.preserve_file_on_drop {
            let _result = fs::remove_file(&self.path);
        }
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

struct RecordingPathGuard {
    path: PathBuf,
    retained: bool,
}

impl RecordingPathGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            retained: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn retain(mut self) -> PathBuf {
        self.retained = true;
        self.path.clone()
    }
}

impl Drop for RecordingPathGuard {
    fn drop(&mut self) {
        if !self.retained {
            let _result = fs::remove_file(&self.path);
        }
    }
}

struct CaptureChildGuard(Option<Child>);

impl CaptureChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("capture child is present")
    }

    fn take(mut self) -> Child {
        self.0.take().expect("capture child is present")
    }
}

impl Drop for CaptureChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.0.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _result = child.kill();
        }
        let _result = child.wait();
    }
}

fn capture_command(device: Option<&str>) -> (&'static str, Command) {
    if command_exists("pw-record") {
        return ("pw-record", pipewire_command(device));
    }
    ("parec", pulseaudio_command(device))
}

fn pipewire_command(device: Option<&str>) -> Command {
    let mut command = Command::new("pw-record");
    command.args([
        "--raw",
        "--format=s16",
        "--rate=16000",
        "--channels=1",
        "--media-category=Capture",
        "--media-role=Communication",
        "--latency=20ms",
    ]);
    if let Some(device) = configured_device(device) {
        command.args(["--target", device]);
    }
    command.arg("-");
    command
}

fn pulseaudio_command(device: Option<&str>) -> Command {
    let mut command = Command::new("parec");
    command.args([
        "--raw",
        "--format=s16le",
        "--rate=16000",
        "--channels=1",
        "--latency-msec=20",
        "--process-time-msec=20",
    ]);
    command.arg(format!(
        "--device={}",
        configured_device(device).unwrap_or("@DEFAULT_SOURCE@")
    ));
    command
}

fn configured_device(device: Option<&str>) -> Option<&str> {
    device.map(str::trim).filter(|value| !value.is_empty())
}

const BYTES_PER_FRAME: usize = 640;

fn frame_metrics(frame: u64, pcm: &[u8]) -> AudioFrameMetrics {
    let mut square_sum = 0_u64;
    let mut peak = 0_u16;
    let mut clipped_samples = 0_u16;
    let mut samples = 0_u16;
    for bytes in pcm.chunks_exact(2) {
        let magnitude = i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs();
        peak = peak.max(magnitude);
        if magnitude >= i16::MAX.unsigned_abs().saturating_sub(32) {
            clipped_samples = clipped_samples.saturating_add(1);
        }
        let value = u64::from(magnitude);
        square_sum = square_sum.saturating_add(value.saturating_mul(value));
        samples = samples.saturating_add(1);
    }
    let rms = square_sum
        .checked_div(u64::from(samples))
        .map(integer_sqrt)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or_default();
    AudioFrameMetrics {
        frame,
        rms,
        peak,
        clipped_samples,
        samples,
    }
}

fn integer_sqrt(value: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    let mut estimate = 1_u64 << value.ilog2().div_ceil(2);
    loop {
        let next = estimate.saturating_add(value / estimate) / 2;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
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
        let (backend, _command) = capture_command(None);
        if command_exists("pw-record") {
            assert_eq!(backend, "pw-record");
        } else {
            assert_eq!(backend, "parec");
        }
    }

    #[test]
    fn places_pipewire_target_before_stdout_path() {
        let command = pipewire_command(Some("alsa_input.test"));
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            &args[args.len() - 3..],
            ["--target", "alsa_input.test", "-"]
        );
    }

    #[test]
    fn selects_one_pulseaudio_device() {
        let command = pulseaudio_command(Some(" source.test "));
        let devices = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .filter(|arg| arg.starts_with("--device="))
            .collect::<Vec<_>>();
        assert_eq!(devices, ["--device=source.test"]);
    }

    #[test]
    fn reports_frame_level_and_clipping_without_retaining_audio() {
        let mut pcm = vec![0_u8; BYTES_PER_FRAME];
        for bytes in pcm.chunks_exact_mut(2) {
            bytes.copy_from_slice(&1_000_i16.to_le_bytes());
        }
        pcm[..2].copy_from_slice(&i16::MAX.to_le_bytes());
        let metrics = frame_metrics(7, &pcm);
        assert_eq!(metrics.frame, 7);
        assert!(metrics.rms >= 999);
        assert_eq!(metrics.peak, i16::MAX.unsigned_abs());
        assert_eq!(metrics.clipped_samples, 1);
        assert_eq!(metrics.samples, 320);
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

    #[test]
    fn pending_recording_path_is_removed_unless_retained() {
        let directory = std::env::temp_dir().join(format!(
            "voxtype-recording-guard-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");

        let abandoned = directory.join("recording-abandoned.pcm");
        fs::write(&abandoned, b"audio").expect("write abandoned recording");
        drop(RecordingPathGuard::new(abandoned.clone()));
        assert!(!abandoned.exists());

        let retained = directory.join("recording-retained.pcm");
        fs::write(&retained, b"audio").expect("write retained recording");
        let retained_path = RecordingPathGuard::new(retained.clone()).retain();
        assert_eq!(retained_path, retained);
        assert!(retained.exists());

        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
