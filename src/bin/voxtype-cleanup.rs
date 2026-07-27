use serde_json::{Value, json};
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use voxtype::client::Client;
use voxtype::qml;
use zbus::blocking::Connection;

const MAX_REPORT_BYTES: usize = 256 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024;

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
    let report = validate_report(&report)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let token = session_token()?;
    let mut qml = launch_qml(listener.local_addr()?.port(), &token)?;
    let mut receipt = None;

    loop {
        if qml.try_wait()?.is_some() {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                handle_connection(&mut stream, &token, &client, &report, &mut receipt);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn launch_qml(port: u16, token: &str) -> io::Result<Child> {
    Command::new(qml::runtime())
        .arg(qml_path())
        .arg("--")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(token)
        .stdin(Stdio::null())
        .spawn()
}

fn handle_connection(
    stream: &mut TcpStream,
    token: &str,
    client: &Client<'_>,
    report: &Value,
    receipt: &mut Option<String>,
) {
    let mut request = Vec::with_capacity(4 * 1024);
    let response = read_request(stream, &mut request).map_or_else(
        |error| Response::error(400, &error.to_string()),
        |parsed| {
            if parsed.token != Some(token) {
                return Response::error(403, "invalid cleanup session");
            }
            route(parsed.method, parsed.path, client, report, receipt)
        },
    );
    let _ = response.write_to(stream);
    request.fill(0);
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    token: Option<&'a str>,
}

fn read_request<'a>(stream: &mut TcpStream, buffer: &'a mut Vec<u8>) -> io::Result<Request<'a>> {
    let mut chunk = [0_u8; 4 * 1024];
    let header_end = loop {
        if buffer.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cleanup request is too large",
            ));
        }
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete cleanup request",
            ));
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let headers = std::str::from_utf8(&buffer[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP request"))?;
    let request_line = headers
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let token = query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "token").then_some(value)
    });
    Ok(Request {
        method,
        path,
        token,
    })
}

fn route(
    method: &str,
    path: &str,
    client: &Client<'_>,
    report: &Value,
    receipt: &mut Option<String>,
) -> Response {
    match (method, path) {
        ("GET", "/report") => Response::json(report),
        ("POST", "/apply") => {
            if receipt.is_some() {
                return Response::error(409, "cleanup suggestions are already applied");
            }
            match client.apply_context_cleanup(&report.to_string()) {
                Ok(value) => {
                    let Ok(parsed) = serde_json::from_str::<Value>(&value) else {
                        return Response::error(502, "daemon returned an invalid cleanup receipt");
                    };
                    *receipt = Some(value);
                    Response::json(&json!({"ok": true, "receipt": parsed}))
                }
                Err(error) => Response::error(409, &error.to_string()),
            }
        }
        ("POST", "/undo") => {
            let Some(value) = receipt.as_deref() else {
                return Response::error(409, "there is no applied cleanup to undo");
            };
            match client.undo_context_cleanup(value) {
                Ok(_) => {
                    *receipt = None;
                    Response::json(&json!({"ok": true, "undone": true}))
                }
                Err(error) => Response::error(409, &error.to_string()),
            }
        }
        _ => Response::error(404, "cleanup endpoint not found"),
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

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn json(value: &Value) -> Self {
        Self {
            status: 200,
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: json!({"ok": false, "error": message})
                .to_string()
                .into_bytes(),
        }
    }

    fn write_to(&self, stream: &mut TcpStream) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            502 => "Bad Gateway",
            _ => "Error",
        };
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            self.status,
            reason,
            self.body.len()
        )?;
        stream.write_all(&self.body)
    }
}

fn session_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
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
