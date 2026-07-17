use serde::{Deserialize, Serialize};
use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use voxtype::qml;

const MAX_REQUEST_BYTES: u64 = 4096;

#[derive(Debug, Deserialize)]
struct OverlayRequest {
    state: String,
    title: String,
    body: String,
    timeout_ms: u32,
    #[serde(default)]
    rms: Option<u16>,
    #[serde(default)]
    adaptive_threshold: Option<u16>,
    #[serde(default)]
    speech_active: bool,
    #[serde(default)]
    clipping_percent: u8,
}

#[derive(Debug, Serialize)]
struct OverlayState<'a> {
    state: &'a str,
    title: &'a str,
    body: &'a str,
    timeout_ms: u32,
    visible: bool,
    updated_ms: u64,
    rms: Option<u16>,
    adaptive_threshold: Option<u16>,
    speech_active: bool,
    clipping_percent: u8,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("voxtype-overlay: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("show") => {
            let request = match arguments.next() {
                Some(state) => legacy_request(state, arguments)?,
                None => read_request(io::stdin().lock())?,
            };
            show(&request)?;
        }
        Some("hide") => hide()?,
        _ => return Err("usage: voxtype-overlay show [STATE TITLE BODY TIMEOUT_MS] | hide".into()),
    }
    Ok(())
}

fn legacy_request(
    state: String,
    mut arguments: impl Iterator<Item = String>,
) -> Result<OverlayRequest, Box<dyn Error>> {
    Ok(OverlayRequest {
        state,
        title: arguments.next().unwrap_or_else(|| "VoxType".to_owned()),
        body: arguments.next().unwrap_or_default(),
        timeout_ms: arguments
            .next()
            .unwrap_or_else(|| "2000".to_owned())
            .parse()?,
        rms: None,
        adaptive_threshold: None,
        speech_active: false,
        clipping_percent: 0,
    })
}

fn read_request(reader: impl Read) -> Result<OverlayRequest, Box<dyn Error>> {
    let mut payload = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut payload)?;
    if payload.len() as u64 > MAX_REQUEST_BYTES {
        return Err("overlay request is too large".into());
    }
    Ok(serde_json::from_slice(&payload)?)
}

fn show(request: &OverlayRequest) -> Result<(), Box<dyn Error>> {
    validate_request(request)?;
    write_state(&OverlayState {
        state: &request.state,
        title: &request.title,
        body: &request.body,
        timeout_ms: request.timeout_ms.min(60_000),
        visible: true,
        updated_ms: now_millis(),
        rms: request.rms,
        adaptive_threshold: request.adaptive_threshold,
        speech_active: request.speech_active,
        clipping_percent: request.clipping_percent.min(100),
    })?;
    ensure_running()?;
    Ok(())
}

fn hide() -> Result<(), Box<dyn Error>> {
    if !state_path()?.exists() {
        return Ok(());
    }
    write_state(&OverlayState {
        state: "idle",
        title: "VoxType",
        body: "",
        timeout_ms: 0,
        visible: false,
        updated_ms: now_millis(),
        rms: None,
        adaptive_threshold: None,
        speech_active: false,
        clipping_percent: 0,
    })?;
    Ok(())
}

fn validate_request(request: &OverlayRequest) -> Result<(), Box<dyn Error>> {
    for (value, maximum) in [
        (request.state.as_str(), 32),
        (request.title.as_str(), 80),
        (request.body.as_str(), 240),
    ] {
        if value.chars().count() > maximum {
            return Err("overlay field is too long".into());
        }
    }
    Ok(())
}

fn write_state(state: &OverlayState<'_>) -> Result<(), Box<dyn Error>> {
    let path = state_path()?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let payload = serde_json::to_vec(state)?;
    if let Err(error) = file
        .write_all(&payload)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, &path))
    {
        let _ = fs::remove_file(temporary);
        return Err(error.into());
    }
    Ok(())
}

fn ensure_running() -> Result<(), Box<dyn Error>> {
    if current_overlay_pid().is_some() {
        return Ok(());
    }
    let qml = qml_path();
    let state = state_path()?;
    let child = Command::new(qml::runtime())
        .arg(qml)
        .arg("--")
        .arg(state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    write_private(pid_path()?, child.id().to_string().as_bytes())?;
    Ok(())
}

fn current_overlay_pid() -> Option<u32> {
    let path = pid_path().ok()?;
    let pid = fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    if is_overlay_process(pid) {
        Some(pid)
    } else {
        let _ = fs::remove_file(path);
        None
    }
}

fn is_overlay_process(pid: u32) -> bool {
    let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    command_line.windows(4).any(|window| window == b"qml6")
        && command_line
            .windows(b"Overlay.qml".len())
            .any(|window| window == b"Overlay.qml")
}

fn write_private(path: PathBuf, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

fn qml_path() -> PathBuf {
    if let Some(path) = env::var_os("VOXTYPE_OVERLAY_QML") {
        return PathBuf::from(path);
    }
    let data = env::var_os("XDG_DATA_HOME").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map_or_else(env::temp_dir, |home| home.join(".local/share"))
        },
        PathBuf::from,
    );
    data.join("voxtype/Overlay.qml")
}

fn runtime_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or("XDG_RUNTIME_DIR is unavailable")?
        .join("voxtype");
    fs::create_dir_all(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    Ok(runtime.join(name))
}

fn state_path() -> Result<PathBuf, Box<dyn Error>> {
    runtime_path("overlay-state.json")
}

fn pid_path() -> Result<PathBuf, Box<dyn Error>> {
    runtime_path("overlay.pid")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_overlay_text() {
        let request = OverlayRequest {
            state: "done".to_owned(),
            title: "VoxType".to_owned(),
            body: "x".repeat(241),
            timeout_ms: 2_000,
            rms: None,
            adaptive_threshold: None,
            speech_active: false,
            clipping_percent: 0,
        };
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn parses_bounded_stdin_update() {
        let request = read_request(
            br#"{"state":"listening","title":"Listening","body":"Speak","timeout_ms":0}"#
                .as_slice(),
        )
        .expect("valid update");
        assert_eq!(request.state, "listening");
        assert_eq!(request.timeout_ms, 0);
    }

    #[test]
    fn parses_structured_audio_metrics() {
        let request = read_request(
            br#"{"state":"listening","title":"Listening","body":"","timeout_ms":0,"rms":420,"adaptive_threshold":300,"speech_active":true,"clipping_percent":2}"#
                .as_slice(),
        )
        .expect("valid telemetry update");
        assert_eq!(request.rms, Some(420));
        assert_eq!(request.adaptive_threshold, Some(300));
        assert!(request.speech_active);
        assert_eq!(request.clipping_percent, 2);
    }
}
