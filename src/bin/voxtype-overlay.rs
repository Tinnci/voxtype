use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use voxtype::qml;

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
            let state = bounded(arguments.next().unwrap_or_else(|| "idle".to_owned()), 32)?;
            let title = bounded(arguments.next().unwrap_or_else(|| "VoxType".to_owned()), 80)?;
            let body = bounded(arguments.next().unwrap_or_default(), 240)?;
            let timeout = arguments
                .next()
                .unwrap_or_else(|| "2000".to_owned())
                .parse::<u32>()?
                .min(60_000);
            show(&state, &title, &body, timeout)?;
        }
        Some("hide") => stop_previous(),
        _ => return Err("usage: voxtype-overlay show STATE TITLE BODY TIMEOUT_MS | hide".into()),
    }
    Ok(())
}

fn show(state: &str, title: &str, body: &str, timeout: u32) -> Result<(), Box<dyn Error>> {
    stop_previous();
    let qml = qml_path();
    let child = Command::new(qml::runtime())
        .arg(qml)
        .arg("--")
        .args([state, title, body, &timeout.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    fs::write(pid_path()?, child.id().to_string())?;
    Ok(())
}

fn stop_previous() {
    let Ok(path) = pid_path() else {
        return;
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        let _ = fs::remove_file(path);
        return;
    };
    let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if command_line.windows(4).any(|window| window == b"qml6")
        && command_line
            .windows(b"Overlay.qml".len())
            .any(|window| window == b"Overlay.qml")
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = fs::remove_file(path);
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

fn pid_path() -> Result<PathBuf, Box<dyn Error>> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or("XDG_RUNTIME_DIR is unavailable")?
        .join("voxtype");
    fs::create_dir_all(&runtime)?;
    Ok(runtime.join("overlay.pid"))
}

fn bounded(value: String, maximum: usize) -> Result<String, Box<dyn Error>> {
    if value.chars().count() > maximum {
        return Err("overlay argument is too long".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_overlay_text() {
        assert!(bounded("x".repeat(241), 240).is_err());
    }
}
