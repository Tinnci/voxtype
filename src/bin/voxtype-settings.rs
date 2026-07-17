use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use voxtype::audio::Recording;
use voxtype::calibration;
use voxtype::client::Client;
use voxtype::config::{
    Config, InsertionBackend, ProfileConfig, ProviderConfig, ProviderQuotaConfig, ReplaySetting,
    config_path, secret_state, store_secret,
};
use voxtype::qml;
use voxtype_core::ProviderId;
use zbus::blocking::Connection;

const MAX_REQUEST_BYTES: usize = 128 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("voxtype-settings: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let _guard = InstanceGuard::acquire()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let token = session_token()?;
    let mut qml = launch_qml(address.port(), &token)?;

    loop {
        if qml.try_wait()?.is_some() {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                handle_connection(&mut stream, &token);
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

fn handle_connection(stream: &mut TcpStream, token: &str) {
    let mut request = Vec::with_capacity(8 * 1024);
    let result = read_request(stream, &mut request).and_then(|parsed| {
        if parsed.token != Some(token) {
            return Ok(Response::error(403, "invalid settings session"));
        }
        route(&parsed)
    });
    let response = result.unwrap_or_else(|error| Response::error(400, &error.to_string()));
    let _ = response.write_to(stream);
    request.fill(0);
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    token: Option<&'a str>,
    body: &'a [u8],
}

fn read_request<'a>(stream: &mut TcpStream, buffer: &'a mut Vec<u8>) -> io::Result<Request<'a>> {
    let mut chunk = [0_u8; 8 * 1024];
    let (header_end, content_length) = loop {
        if buffer.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings request is too large",
            ));
        }
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete settings request",
            ));
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = position + 4;
            let headers = std::str::from_utf8(&buffer[..position])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP headers"))?;
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end, length);
        }
    };
    if header_end.saturating_add(content_length) > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings request body is too large",
        ));
    }
    while buffer.len() < header_end + content_length {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete settings request body",
            ));
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
    let headers = std::str::from_utf8(&buffer[..header_end - 4])
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
        body: &buffer[header_end..header_end + content_length],
    })
}

fn route(request: &Request<'_>) -> io::Result<Response> {
    match (request.method, request.path) {
        ("GET", "/state") => settings_state().map(|value| Response::json(&value)),
        ("POST", "/general") => save_general(request.body),
        ("POST", "/calibrate") => calibrate_microphone(),
        ("POST", "/provider") => create_provider(request.body),
        ("POST", "/open-config") => open_config(),
        ("POST", path) if path.starts_with("/provider/") => {
            save_provider(&percent_decode(&path[10..])?, request.body)
        }
        ("POST", path) if path.starts_with("/quota/") => {
            save_quota(&percent_decode(&path[7..])?, request.body)
        }
        ("POST", path) if path.starts_with("/secret/") => {
            save_secret(&percent_decode(&path[8..])?, request.body)
        }
        _ => Ok(Response::error(404, "settings endpoint not found")),
    }
}

fn calibrate_microphone() -> io::Result<Response> {
    let result = calibrate_microphone_inner();
    if result.is_err() {
        calibration_phase(
            "Microphone calibration failed",
            "Check the selected device and try again",
            3_000,
        );
    }
    result
}

fn calibrate_microphone_inner() -> io::Result<Response> {
    const SILENCE_MILLIS: u64 = 1_500;
    const SPEECH_MILLIS: u64 = 3_000;
    let config = Config::load_or_create().map_err(domain_io)?;
    calibration_phase(
        "Microphone calibration",
        "Stay quiet while VoxType measures room noise",
        0,
    );
    let recording = Recording::start_with_device(Some(config.audio.device.as_str()))?;
    thread::sleep(Duration::from_millis(SILENCE_MILLIS));
    calibration_phase(
        "Microphone calibration",
        "Speak a normal sentence until the recording finishes",
        0,
    );
    thread::sleep(Duration::from_millis(SPEECH_MILLIS));
    let recording_path = recording.path().to_owned();
    let recording = match recording.stop() {
        Ok(recording) => recording,
        Err(error) => {
            let _ = fs::remove_file(recording_path);
            return Err(error);
        }
    };
    let pcm = fs::read(&recording.path);
    let _ = fs::remove_file(&recording.path);
    let pcm = pcm?;
    let silence_bytes = usize::try_from(SILENCE_MILLIS.saturating_mul(32))
        .unwrap_or(usize::MAX)
        .min(pcm.len());
    let result = calibration::analyze(&pcm[..silence_bytes], &pcm[silence_bytes..])
        .map_err(io::Error::other)?;
    calibration_phase(
        "Microphone calibration complete",
        if result.can_apply {
            "The suggested threshold is ready for review"
        } else {
            "The sample was not reliable enough to change the threshold"
        },
        3_000,
    );
    Ok(Response::json(&json!({
        "capture_backend": recording.backend,
        "configured_device": if config.audio.device.is_empty() { "default" } else { config.audio.device.as_str() },
        "noise_floor": result.noise_p50,
        "noise_p20": result.noise_p20,
        "noise_p50": result.noise_p50,
        "noise_p95": result.noise_p95,
        "average_rms": result.speech_p50,
        "speech_p50": result.speech_p50,
        "speech_p95": result.speech_p95,
        "peak": result.peak,
        "adaptive_threshold": result.suggested_threshold,
        "suggested_threshold": result.suggested_threshold,
        "snr_db": result.snr_db,
        "clipping_ratio": result.clipping_ratio,
        "speech_ratio": result.speech_ratio,
        "confidence": result.confidence.as_str(),
        "reason": result.reason.as_str(),
        "can_apply": result.can_apply,
    })))
}

fn calibration_phase(title: &str, body: &str, timeout_millis: u32) {
    let payload = json!({
        "state": "calibration",
        "title": title,
        "body": body,
        "timeout_ms": timeout_millis,
    })
    .to_string();
    let Ok(mut child) = Command::new("voxtype-overlay")
        .arg("show")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
}

fn settings_state() -> io::Result<Value> {
    let config = Config::load_or_create().map_err(domain_io)?;
    let onboarding_needed = needs_provider_onboarding(&config);
    let usage = Connection::session()
        .and_then(|connection| {
            let client = Client::connect(&connection)?;
            client.usage_status()
        })
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok())
        .unwrap_or_else(|| json!({"scope": "unavailable", "providers": {}}));
    let provider_health = Connection::session()
        .and_then(|connection| {
            let client = Client::connect(&connection)?;
            client.provider_status()
        })
        .unwrap_or_default();
    let providers = config
        .providers
        .iter()
        .map(|(id, provider)| {
            let live_usage = usage
                .pointer(&format!("/providers/{}/usage", json_pointer_escape(id)))
                .cloned()
                .unwrap_or_else(empty_usage);
            provider_state(
                id,
                provider,
                &live_usage,
                config.quotas.get(id),
                provider_health_for(&provider_health, id),
            )
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "config_path": config_path().map_err(domain_io)?,
        "usage_scope": usage.get("scope").and_then(Value::as_str).unwrap_or("unavailable"),
        "onboarding_needed": onboarding_needed,
        "general": {
            "default_profile": config.default_profile,
            "profiles": config.profiles.keys().collect::<Vec<_>>(),
            "insertion_backend": insertion_backend_name(config.desktop.insertion_backend),
            "restore_clipboard": config.desktop.restore_clipboard,
            "retain_recordings": config.desktop.retain_recordings,
            "transcript_history_enabled": config.desktop.transcript_history_enabled,
            "vad_enabled": config.audio.vad_enabled,
            "audio_device": config.audio.device,
            "vad_rms_threshold": config.audio.vad_rms_threshold,
            "vad_minimum_voiced_frames": config.audio.vad_minimum_voiced_frames,
            "minimum_duration_millis": config.audio.minimum_duration_millis,
            "maximum_duration_seconds": config.audio.maximum_duration_seconds,
        },
        "providers": providers,
    }))
}

fn provider_state(
    id: &str,
    provider: &ProviderConfig,
    usage: &Value,
    quota: Option<&ProviderQuotaConfig>,
    health: &str,
) -> Value {
    let (kind, endpoint, model, secret_ref, secret_state, timeout_seconds, smart_format) =
        match provider {
            ProviderConfig::Mock { .. } => {
                ("mock", "", "", "", "not-required", Value::Null, Value::Null)
            }
            ProviderConfig::OpenaiCompatible {
                endpoint,
                model,
                secret,
                timeout_seconds,
                ..
            } => (
                "openai-compatible",
                endpoint.as_str(),
                model.as_str(),
                secret.as_str(),
                secret_state(secret),
                json!(timeout_seconds),
                Value::Null,
            ),
            ProviderConfig::Deepgram {
                endpoint,
                model,
                secret,
                timeout_seconds,
                smart_format,
            } => (
                "deepgram",
                endpoint.as_str(),
                model.as_str(),
                secret.as_str(),
                secret_state(secret),
                json!(timeout_seconds),
                json!(smart_format),
            ),
            ProviderConfig::Command {
                program,
                timeout_seconds,
                ..
            } => (
                "command",
                program.as_str(),
                "",
                "",
                "not-required",
                json!(timeout_seconds),
                Value::Null,
            ),
        };
    json!({
        "id": id,
        "kind": kind,
        "endpoint": endpoint,
        "model": model,
        "secret_ref": secret_ref,
        "secret_state": secret_state,
        "readiness": provider_readiness(kind, secret_state, health),
        "health": health,
        "timeout_seconds": timeout_seconds,
        "smart_format": smart_format,
        "usage": usage,
        "quota": quota.cloned().unwrap_or_default(),
    })
}

fn provider_health_for<'a>(status: &'a str, provider_id: &str) -> &'a str {
    status
        .split_whitespace()
        .find_map(|entry| {
            let (id, health) = entry.split_once(":available=")?;
            (id == provider_id).then_some(health)
        })
        .unwrap_or("unknown")
}

fn provider_readiness(kind: &str, secret_state: &str, health: &str) -> &'static str {
    if kind == "mock" {
        return "demo";
    }
    if matches!(secret_state, "missing" | "unavailable") {
        return "setup-needed";
    }
    if health.starts_with("false") {
        return "unavailable";
    }
    if health.starts_with("true") && provider_failure_count(health) > 0 {
        return "degraded";
    }
    if health.starts_with("true") {
        return "healthy";
    }
    if kind == "command" {
        "configured"
    } else {
        "unknown"
    }
}

fn provider_failure_count(health: &str) -> u32 {
    health
        .split(',')
        .find_map(|field| field.strip_prefix("failures="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn needs_provider_onboarding(config: &Config) -> bool {
    config
        .providers
        .values()
        .all(|provider| matches!(provider, ProviderConfig::Mock { .. }))
}

#[derive(Deserialize)]
struct CreateProviderUpdate {
    id: String,
    kind: String,
    endpoint: String,
    model: String,
    language: String,
    make_default: bool,
}

fn create_provider(body: &[u8]) -> io::Result<Response> {
    let update: CreateProviderUpdate = serde_json::from_slice(body).map_err(invalid_input)?;
    let mut config = Config::load_or_create().map_err(domain_io)?;
    if let Err(message) = apply_provider_create(&mut config, &update) {
        return Ok(Response::error(400, &message));
    }
    persist_and_reload(&config)
}

fn apply_provider_create(config: &mut Config, update: &CreateProviderUpdate) -> Result<(), String> {
    let id = ProviderId::new(update.id.trim())
        .map_err(|error| error.message().to_owned())?
        .to_string();
    let language = update.language.trim();
    if language.is_empty() || language.len() > 32 || language.chars().any(char::is_control) {
        return Err("language must contain 1 to 32 characters".to_owned());
    }
    if config.providers.contains_key(&id) || config.profiles.contains_key(&id) {
        return Err("provider or profile ID already exists".to_owned());
    }
    let secret = format!("{id}-api-key");
    let provider = match update.kind.as_str() {
        "openai-compatible" => ProviderConfig::OpenaiCompatible {
            endpoint: update.endpoint.trim().to_owned(),
            model: update.model.trim().to_owned(),
            secret,
            timeout_seconds: 30,
        },
        "deepgram" => ProviderConfig::Deepgram {
            endpoint: update.endpoint.trim().to_owned(),
            model: update.model.trim().to_owned(),
            secret,
            timeout_seconds: 30,
            smart_format: true,
        },
        _ => return Err("unsupported provider kind".to_owned()),
    };
    config.providers.insert(id.clone(), provider);
    config.profiles.insert(
        id.clone(),
        ProfileConfig {
            primary: id.clone(),
            fallbacks: Vec::new(),
            language: language.to_owned(),
            replay: ReplaySetting::Never,
        },
    );
    if update.make_default {
        config.default_profile = id;
    }
    config
        .validate()
        .map_err(|error| error.message().to_owned())
}

fn empty_usage() -> Value {
    json!({
        "attempts": 0,
        "requests": 0,
        "successes": 0,
        "failures": 0,
        "audio_millis": 0,
        "token_reports": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "reported_tokens": 0,
    })
}

#[derive(Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors independent settings checkboxes in the wire payload.
struct GeneralUpdate {
    default_profile: String,
    audio_device: String,
    insertion_backend: String,
    restore_clipboard: bool,
    retain_recordings: bool,
    transcript_history_enabled: bool,
    vad_enabled: bool,
    vad_rms_threshold: u16,
    vad_minimum_voiced_frames: u32,
    minimum_duration_millis: u64,
    maximum_duration_seconds: u64,
}

fn save_general(body: &[u8]) -> io::Result<Response> {
    let update: GeneralUpdate = serde_json::from_slice(body).map_err(invalid_input)?;
    let mut config = Config::load_or_create().map_err(domain_io)?;
    config.default_profile = update.default_profile;
    update
        .audio_device
        .trim()
        .clone_into(&mut config.audio.device);
    config.desktop.insertion_backend = match update.insertion_backend.as_str() {
        "fcitx" => InsertionBackend::Fcitx,
        "clipboard" => InsertionBackend::Clipboard,
        "copy" => InsertionBackend::Copy,
        "auto" => InsertionBackend::Auto,
        _ => return Ok(Response::error(400, "unsupported insertion backend")),
    };
    config.desktop.restore_clipboard = update.restore_clipboard;
    config.desktop.retain_recordings = update.retain_recordings;
    config.desktop.transcript_history_enabled = update.transcript_history_enabled;
    config.audio.vad_enabled = update.vad_enabled;
    config.audio.vad_rms_threshold = update.vad_rms_threshold;
    config.audio.vad_minimum_voiced_frames = update.vad_minimum_voiced_frames;
    config.audio.minimum_duration_millis = update.minimum_duration_millis;
    config.audio.maximum_duration_seconds = update.maximum_duration_seconds;
    persist_and_reload(&config)
}

#[derive(Deserialize)]
struct RestProviderUpdate {
    endpoint: String,
    model: String,
    timeout_seconds: u64,
    smart_format: Option<bool>,
}

fn save_provider(provider: &str, body: &[u8]) -> io::Result<Response> {
    let update: RestProviderUpdate = serde_json::from_slice(body).map_err(invalid_input)?;
    let mut config = Config::load_or_create().map_err(domain_io)?;
    let Some(provider_config) = config.providers.get_mut(provider) else {
        return Ok(Response::error(404, "provider is not configured"));
    };
    match provider_config {
        ProviderConfig::OpenaiCompatible {
            endpoint,
            model,
            timeout_seconds,
            ..
        } => {
            *endpoint = update.endpoint;
            *model = update.model;
            *timeout_seconds = update.timeout_seconds;
        }
        ProviderConfig::Deepgram {
            endpoint,
            model,
            timeout_seconds,
            smart_format,
            ..
        } => {
            *endpoint = update.endpoint;
            *model = update.model;
            *timeout_seconds = update.timeout_seconds;
            if let Some(value) = update.smart_format {
                *smart_format = value;
            }
        }
        ProviderConfig::Mock { .. } | ProviderConfig::Command { .. } => {
            return Ok(Response::error(
                400,
                "this provider does not have editable cloud API settings",
            ));
        }
    }
    persist_and_reload(&config)
}

#[derive(Deserialize)]
struct QuotaUpdate {
    #[serde(rename = "request_limit")]
    requests: Option<u64>,
    #[serde(rename = "audio_seconds_limit")]
    audio_seconds: Option<u64>,
    #[serde(rename = "token_limit")]
    tokens: Option<u64>,
}

fn save_quota(provider: &str, body: &[u8]) -> io::Result<Response> {
    let update: QuotaUpdate = serde_json::from_slice(body).map_err(invalid_input)?;
    let mut config = Config::load_or_create().map_err(domain_io)?;
    if !config.providers.contains_key(provider) {
        return Ok(Response::error(404, "provider is not configured"));
    }
    let quota = ProviderQuotaConfig {
        request_limit: update.requests,
        audio_seconds_limit: update.audio_seconds,
        token_limit: update.tokens,
    };
    if quota.request_limit.is_none()
        && quota.audio_seconds_limit.is_none()
        && quota.token_limit.is_none()
    {
        config.quotas.remove(provider);
    } else {
        config.quotas.insert(provider.to_owned(), quota);
    }
    persist_and_reload(&config)
}

fn save_secret(name: &str, body: &[u8]) -> io::Result<Response> {
    let config = Config::load_or_create().map_err(domain_io)?;
    let known = config.providers.values().any(|provider| {
        matches!(provider, ProviderConfig::OpenaiCompatible { secret, .. } | ProviderConfig::Deepgram { secret, .. } if secret == name)
    });
    if !known {
        return Ok(Response::error(404, "secret reference is not configured"));
    }
    if body.is_empty() || body.len() > 16 * 1024 || body.contains(&0) {
        return Ok(Response::error(
            400,
            "API key must contain 1 to 16384 bytes",
        ));
    }
    store_secret(name, body).map_err(domain_io)?;
    Ok(Response::json(&json!({"ok": true})))
}

fn persist_and_reload(config: &Config) -> io::Result<Response> {
    config.save().map_err(domain_io)?;
    let reload = Connection::session().and_then(|connection| {
        let client = Client::connect(&connection)?;
        client.reload_configuration()
    });
    Ok(Response::json(&json!({
        "ok": true,
        "daemon_reloaded": reload.is_ok(),
    })))
}

fn open_config() -> io::Result<Response> {
    let path = config_path().map_err(domain_io)?;
    Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(Response::json(&json!({"ok": true})))
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

fn insertion_backend_name(backend: InsertionBackend) -> &'static str {
    match backend {
        InsertionBackend::Fcitx => "fcitx",
        InsertionBackend::Clipboard => "clipboard",
        InsertionBackend::Copy => "copy",
        InsertionBackend::Auto => "auto",
    }
}

fn qml_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VOXTYPE_SETTINGS_QML") {
        return PathBuf::from(path);
    }
    data_home().join("voxtype/Settings.qml")
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from("/tmp"),
                |home| PathBuf::from(home).join(".local/share"),
            )
        },
        PathBuf::from,
    )
}

fn runtime_directory() -> io::Result<PathBuf> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unavailable"))?
        .join("voxtype");
    fs::create_dir_all(&path)?;
    Ok(path)
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

fn percent_decode(value: &str) -> io::Result<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid percent encoding",
                ));
            }
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"))
}

fn hex(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid percent encoding",
        )),
    }
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn domain_io(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn invalid_input(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

struct InstanceGuard {
    path: PathBuf,
}

impl InstanceGuard {
    fn acquire() -> io::Result<Self> {
        let path = runtime_directory()?.join("settings.pid");
        if let Ok(pid) = fs::read_to_string(&path).and_then(|value| {
            value
                .trim()
                .parse::<u32>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }) {
            let command_line = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if command_line
                .windows(b"voxtype-settings".len())
                .any(|window| window == b"voxtype-settings")
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the settings panel is already open",
                ));
            }
            let _ = fs::remove_file(&path);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        write!(file, "{}", std::process::id())?;
        Ok(Self { path })
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_provider_and_secret_names() {
        assert_eq!(
            percent_decode("cloud%2Fprimary").expect("decode"),
            "cloud/primary"
        );
    }

    #[test]
    fn escapes_json_pointer_segments() {
        assert_eq!(json_pointer_escape("a/b~c"), "a~1b~0c");
    }

    #[test]
    fn creates_a_real_provider_and_profile() {
        let mut config: Config = toml::from_str(
            r#"schema_version = 1
default_profile = "demo"
[desktop]
[audio]
[profiles.demo]
primary = "demo"
[providers.demo]
kind = "mock"
text = "demo"
"#,
        )
        .expect("fixture config");
        let update = CreateProviderUpdate {
            id: "work-asr".to_owned(),
            kind: "deepgram".to_owned(),
            endpoint: "https://api.deepgram.com/v1/listen".to_owned(),
            model: "nova-3".to_owned(),
            language: "zh".to_owned(),
            make_default: true,
        };
        apply_provider_create(&mut config, &update).expect("provider creation");
        assert_eq!(config.default_profile, "work-asr");
        assert_eq!(config.profiles["work-asr"].primary, "work-asr");
        assert!(matches!(
            config.providers["work-asr"],
            ProviderConfig::Deepgram { .. }
        ));
    }

    #[test]
    fn exposes_provider_readiness_without_calling_configured_count_ready() {
        assert_eq!(
            provider_health_for(
                "mock:available=true cloud:available=false,failures=3",
                "cloud"
            ),
            "false,failures=3"
        );
        assert_eq!(
            provider_health_for("cloud:available=true,failures=1", "missing"),
            "unknown"
        );
        assert_eq!(
            provider_readiness("mock", "not-required", "unknown"),
            "demo"
        );
        assert_eq!(
            provider_readiness("deepgram", "missing", "unknown"),
            "setup-needed"
        );
        assert_eq!(
            provider_readiness("deepgram", "configured", "false,failures=3"),
            "unavailable"
        );
        assert_eq!(
            provider_readiness("deepgram", "configured", "true,failures=1"),
            "degraded"
        );
        assert_eq!(
            provider_readiness("deepgram", "configured", "true,failures=0"),
            "healthy"
        );
        assert_eq!(
            provider_readiness("command", "not-required", "unknown"),
            "configured"
        );
    }
}
