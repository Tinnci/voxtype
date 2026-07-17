use serde_json::Value;
use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use voxtype::client::Client;
use voxtype::qml;
use zbus::blocking::Connection;

const MAX_REPORT_BYTES: usize = 256 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("voxtype-cleanup: {error}");
        let message = error.to_string();
        let _ = Command::new("notify-send")
            .args([
                "--app-name=VoxType",
                "VoxType text cleanup",
                message.as_str(),
            ])
            .spawn();
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mode = env::args().nth(1).unwrap_or_else(|| "context".to_owned());
    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    let report = match mode.as_str() {
        "context" => client.check_context_grammar()?,
        "last" => client.check_last_grammar()?,
        _ => return Err("usage: voxtype-cleanup [context|last]".into()),
    };
    validate_report(&report)?;
    let path = write_private_report(report.as_bytes())?;
    let cleanup = ReportCleanup(path.clone());
    let status = Command::new(qml::runtime())
        .arg(qml_path())
        .arg("--")
        .arg(&path)
        .stdin(Stdio::null())
        .status()?;
    drop(cleanup);
    if status.success() {
        Ok(())
    } else {
        Err(format!("QML review window exited with {status}").into())
    }
}

fn validate_report(report: &str) -> Result<Value, Box<dyn Error>> {
    if report.len() > MAX_REPORT_BYTES {
        return Err("cleanup report is too large".into());
    }
    let value: Value = serde_json::from_str(report)?;
    if value.get("schema").and_then(Value::as_u64) != Some(1)
        || value.get("clean").and_then(Value::as_bool).is_none()
        || value.get("suggested").and_then(Value::as_str).is_none()
        || value.get("edits").and_then(Value::as_array).is_none()
    {
        return Err("cleanup report has an unsupported schema".into());
    }
    Ok(value)
}

fn write_private_report(payload: &[u8]) -> io::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unavailable"))?
        .join("voxtype");
    fs::create_dir_all(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = runtime.join(format!(
        "cleanup-report-{}-{nonce}.json",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    if let Err(error) = file.write_all(payload).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

fn qml_path() -> PathBuf {
    if let Some(path) = env::var_os("VOXTYPE_CLEANUP_QML") {
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
    data.join("voxtype/Cleanup.qml")
}

struct ReportCleanup(PathBuf);

impl Drop for ReportCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_versioned_cleanup_contract() {
        let report = r#"{
            "schema":1,
            "clean":false,
            "suggested":"你好，世界！",
            "edits":[]
        }"#;
        assert!(validate_report(report).is_ok());
    }

    #[test]
    fn rejects_legacy_text_and_unknown_schema() {
        assert!(validate_report("clean=false suggested=text").is_err());
        assert!(validate_report(r#"{"schema":2,"clean":true,"suggested":"","edits":[]}"#).is_err());
    }
}
