use std::env;
use std::error::Error;
use std::io::{self, Read};
use voxtype::audio::Recording;
use voxtype::client::Client;
use voxtype::config::{Config, ProviderConfig, config_path, store_secret};
use voxtype::fcitx::FcitxBridge;
use voxtype::vad::{self, VadConfig};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const KGLOBALACCEL_NAME: &str = "org.kde.kglobalaccel";
const KGLOBALACCEL_PATH: &str = "/kglobalaccel";
const KGLOBALACCEL_INTERFACE: &str = "org.kde.KGlobalAccel";
const KGLOBALACCEL_COMPONENT_INTERFACE: &str = "org.kde.kglobalaccel.Component";

type KGlobalShortcutInfo = (
    String,
    String,
    String,
    String,
    String,
    String,
    Vec<i32>,
    Vec<i32>,
);

#[derive(Clone, Copy)]
struct ExpectedShortcut {
    label: &'static str,
    component_path: &'static str,
    component_id: &'static str,
    action_id: &'static str,
}

const EXPECTED_SHORTCUTS: [ExpectedShortcut; 5] = [
    ExpectedShortcut {
        label: "toggle",
        component_path: "/component/io_github_tinnci_VoxType_desktop",
        component_id: "io.github.tinnci.VoxType.desktop",
        action_id: "_launch",
    },
    ExpectedShortcut {
        label: "cancel",
        component_path: "/component/io_github_tinnci_VoxType_desktop",
        component_id: "io.github.tinnci.VoxType.desktop",
        action_id: "Cancel",
    },
    ExpectedShortcut {
        label: "start",
        component_path: "/component/io_github_tinnci_VoxType_desktop",
        component_id: "io.github.tinnci.VoxType.desktop",
        action_id: "Start",
    },
    ExpectedShortcut {
        label: "stop",
        component_path: "/component/io_github_tinnci_VoxType_desktop",
        component_id: "io.github.tinnci.VoxType.desktop",
        action_id: "Stop",
    },
    ExpectedShortcut {
        label: "grammar",
        component_path: "/component/io_github_tinnci_VoxType_Grammar_desktop",
        component_id: "io.github.tinnci.VoxType.Grammar.desktop",
        action_id: "_launch",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShortcutState {
    Ok,
    MissingComponent,
    MissingAction,
    Inactive,
    Unbound,
    Conflict,
}

impl ShortcutState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::MissingComponent => "missing-component",
            Self::MissingAction => "missing-action",
            Self::Inactive => "inactive",
            Self::Unbound => "unbound",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Debug)]
struct ShortcutDiagnostic {
    label: &'static str,
    state: ShortcutState,
    current: Vec<i32>,
    defaults: Vec<i32>,
    conflicts: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("voxtype: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "status".to_owned());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_help();
        return Ok(());
    }

    if command == "config" {
        return config_command(arguments.next().as_deref().unwrap_or("path"));
    }
    if command == "secret" {
        return secret_command(
            arguments.next().as_deref().unwrap_or(""),
            arguments.next().as_deref().unwrap_or(""),
        );
    }
    if command == "doctor" {
        return doctor_command(arguments.next().as_deref());
    }

    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    match command.as_str() {
        "status" => println!("{}", client.status()?),
        "providers" => println!("{}", client.provider_status()?),
        "usage" => println!("{}", client.usage_status()?),
        "grammar" => grammar_command(&client, arguments.next().as_deref().unwrap_or("last"))?,
        "fcitx-focus" => {
            if client.status()? != "idle" {
                return Err("fcitx focus probing requires an idle daemon".into());
            }
            let target = FcitxBridge.probe()?;
            println!("program={} frontend={}", target.program, target.frontend);
        }
        "fcitx-context" => {
            let context = FcitxBridge.context()?;
            println!(
                "program={} frontend={} generation={} chars={} cursor={} anchor={} selected_chars={} truncated={} capabilities={}",
                context.target.program,
                context.target.frontend,
                context.generation,
                context.text.chars().count(),
                context.cursor,
                context.anchor,
                context.cursor.abs_diff(context.anchor),
                context.truncated,
                context.capabilities.join(",")
            );
        }
        "fcitx-insert-test" => {
            if client.status()? != "idle" {
                return Err("fcitx insertion testing requires an idle daemon".into());
            }
            let text = arguments.collect::<Vec<_>>().join(" ");
            let target = FcitxBridge.commit_test(&text)?;
            println!(
                "dispatched=true program={} frontend={}",
                target.program, target.frontend
            );
        }
        "start" => println!(
            "{}",
            client.start(arguments.next().as_deref().unwrap_or(""))?
        ),
        "stop" => {
            let first = arguments.next().unwrap_or_default();
            if first == "--wait" {
                let result = client.stop_wait(arguments.next().as_deref().unwrap_or(""))?;
                println!(
                    "session={} outcome={} error_code={} backend={} chars={}",
                    result.session,
                    result.outcome,
                    result.error_code,
                    result.backend,
                    result.char_count
                );
            } else {
                println!("{}", client.stop(&first)?);
            }
        }
        "toggle" => println!(
            "{}",
            client.toggle(arguments.next().as_deref().unwrap_or(""))?
        ),
        "cancel" => client.cancel(arguments.next().as_deref().unwrap_or(""))?,
        "reset" => client.reset()?,
        "insert-test" => println!(
            "{}",
            client.insert_test(&arguments.collect::<Vec<_>>().join(" "))?
        ),
        "reload" => client.reload_configuration()?,
        unknown => return Err(format!("unknown command: {unknown}").into()),
    }
    Ok(())
}

fn print_help() {
    println!(
        "VoxType CLI\n\nUsage:\n  voxtype status\n  voxtype providers\n  voxtype usage\n  voxtype grammar context|last|show|history|clear\n  voxtype fcitx-focus\n  voxtype fcitx-context\n  voxtype fcitx-insert-test TEXT\n  voxtype start [PROFILE]\n  voxtype stop [SESSION]\n  voxtype stop --wait [SESSION]\n  voxtype toggle [PROFILE]\n  voxtype cancel [SESSION]\n  voxtype reset\n  voxtype reload\n  voxtype doctor [audio|shortcut|insertion|provider|all]\n  voxtype insert-test TEXT\n  voxtype config path|validate\n  voxtype secret set NAME"
    );
}

fn grammar_command(client: &Client<'_>, action: &str) -> Result<(), Box<dyn Error>> {
    match action {
        "context" => println!("{}", client.check_context_grammar()?),
        "last" => println!("{}", client.check_last_grammar()?),
        "show" => println!("{}", client.last_transcript()?),
        "history" => {
            for (index, text) in client.transcript_history()?.iter().enumerate() {
                println!("{}\t{}", index + 1, text);
            }
        }
        "clear" => client.clear_history()?,
        _ => return Err("usage: voxtype grammar context|last|show|history|clear".into()),
    }
    Ok(())
}

fn doctor_command(section: Option<&str>) -> Result<(), Box<dyn Error>> {
    match section {
        Some("audio") => return doctor_audio(),
        Some("shortcut") => {
            print_shortcut_diagnostics();
            return Ok(());
        }
        Some("insertion") => {
            require_idle_daemon("insertion doctor")?;
            let target = FcitxBridge.probe()?;
            println!(
                "insertion.fcitx=ok program={} frontend={}",
                target.program, target.frontend
            );
            return Ok(());
        }
        Some("provider") => return doctor_provider(),
        Some("all") | None => {}
        Some(_) => {
            return Err("usage: voxtype doctor [audio|shortcut|insertion|provider|all]".into());
        }
    }
    let config = Config::load_or_create()?;
    println!(
        "config=ok schema={} profiles={} providers={}",
        config.schema_version,
        config.profiles.len(),
        config.providers.len()
    );
    match FcitxBridge.ping() {
        Ok(()) => println!("fcitx5-bridge=ok"),
        Err(error) => println!("fcitx5-bridge=unavailable code={}", error.code()),
    }
    println!(
        "session.wayland={}",
        std::env::var_os("WAYLAND_DISPLAY").is_some()
    );
    println!(
        "input.xmodifiers={}",
        std::env::var("XMODIFIERS").unwrap_or_else(|_| "unset".to_owned())
    );
    for (name, value) in [
        ("QT_IM_MODULE", "QT_IM_MODULE"),
        ("GTK_IM_MODULE", "GTK_IM_MODULE"),
    ] {
        println!(
            "input.{name}={}",
            std::env::var(value).unwrap_or_else(|_| "unset".to_owned())
        );
    }
    let kxkbrc = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".config/kxkbrc"));
    match kxkbrc.and_then(|path| std::fs::read_to_string(path).ok()) {
        Some(contents) => {
            let model = config_value(&contents, "Model").unwrap_or("unset");
            let layout = config_value(&contents, "LayoutList").unwrap_or("unset");
            println!("keyboard.xkb_model={model}");
            println!("keyboard.xkb_layout={layout}");
        }
        None => println!("keyboard.xkb=unavailable"),
    }
    for service in [
        "voxtyped.service",
        "voxtype-tray.service",
        "hyprwhspr.service",
        "ydotool.service",
    ] {
        println!("service.{service}={}", user_service_state(service));
    }
    print_shortcut_diagnostics();
    for command in [
        "parec",
        "curl",
        "wl-copy",
        "wl-paste",
        "ydotool",
        "notify-send",
        "qdbus6",
        "qml6",
        "secret-tool",
        "xdg-open",
        "systemsettings",
    ] {
        if command_exists(command) {
            println!("command.{command}=ok");
        } else {
            println!("command.{command}=missing");
        }
    }
    Ok(())
}

fn doctor_provider() -> Result<(), Box<dyn Error>> {
    let config = Config::load_or_create()?;
    for (id, provider) in &config.providers {
        let kind = match provider {
            ProviderConfig::Mock { .. } => "mock",
            ProviderConfig::OpenaiCompatible { secret, .. } => {
                let secret_state = match voxtype::config::lookup_secret(secret) {
                    Ok(_) => "ok",
                    Err(error) if error.code() == "secret.not_found" => "missing",
                    Err(_) => "unavailable",
                };
                println!("provider.{id}.secret={secret_state}");
                "openai-compatible"
            }
            ProviderConfig::Deepgram { secret, .. } => {
                let secret_state = match voxtype::config::lookup_deepgram_secret(secret) {
                    Ok(_) => "ok",
                    Err(error) if error.code() == "secret.not_found" => "missing",
                    Err(_) => "unavailable",
                };
                println!("provider.{id}.secret={secret_state}");
                "deepgram"
            }
            ProviderConfig::Command { .. } => "command",
        };
        println!("provider.{id}=configured kind={kind}");
    }
    if let Ok(connection) = Connection::session() {
        if let Ok(client) = Client::connect(&connection) {
            println!("provider.health={}", client.provider_status()?);
        }
    }
    Ok(())
}

fn doctor_audio() -> Result<(), Box<dyn Error>> {
    require_idle_daemon("audio doctor")?;
    let config = Config::load_or_create()?;
    let recording = Recording::start_with_device(Some(config.audio.device.as_str()))?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    let result = recording.stop()?;
    let vad = vad::analyze_file(
        &result.path,
        VadConfig {
            rms_threshold: config.audio.vad_rms_threshold,
            minimum_voiced_frames: config.audio.vad_minimum_voiced_frames,
        },
    )?;
    let cleanup = std::fs::remove_file(&result.path);
    cleanup?;
    if result.bytes == 0 {
        return Err("audio capture produced no PCM data".into());
    }
    let level = microphone_level_status(vad.peak);
    let speech_ratio = if vad.total_frames == 0 {
        0.0
    } else {
        f64::from(vad.voiced_frames) / f64::from(vad.total_frames)
    };
    let suggested_threshold = vad.noise_floor.saturating_mul(2).saturating_add(80);
    println!(
        "audio.capture=ok backend={} bytes={} duration_ms={} format=s16le rate=16000 channels=1 rms={} peak={} level={} noise_floor={} threshold={} suggested_threshold={} speech_ratio={speech_ratio:.3} clipping={}",
        result.backend,
        result.bytes,
        result.duration_millis,
        vad.average_rms,
        vad.peak,
        level,
        vad.noise_floor,
        vad.adaptive_threshold,
        suggested_threshold,
        vad.peak >= 32_000,
    );
    Ok(())
}

const fn microphone_level_status(peak: u16) -> &'static str {
    if peak >= 32_000 {
        "clipping"
    } else if peak < 500 {
        "too-quiet"
    } else {
        "ok"
    }
}

fn require_idle_daemon(operation: &str) -> Result<(), Box<dyn Error>> {
    if let Ok(connection) = Connection::session() {
        if let Ok(client) = Client::connect(&connection) {
            let status = client.status()?;
            if status != "idle" {
                return Err(
                    format!("{operation} requires an idle daemon; current state={status}").into(),
                );
            }
        }
    }
    Ok(())
}

fn print_shortcut_diagnostics() {
    let diagnostics = match kglobalaccel_diagnostics() {
        Ok(diagnostics) => diagnostics,
        Err(error) => {
            println!("shortcut.kglobalaccel=unavailable");
            println!("shortcut.error={error}");
            return;
        }
    };
    let overall = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.state == ShortcutState::Conflict)
    {
        "conflict"
    } else if diagnostics
        .iter()
        .all(|diagnostic| diagnostic.state == ShortcutState::Ok)
    {
        "ok"
    } else {
        "incomplete"
    };
    println!("shortcut.kglobalaccel={overall}");
    for diagnostic in diagnostics {
        let current = format_shortcuts(&diagnostic.current);
        let defaults = format_shortcuts(&diagnostic.defaults);
        if diagnostic.conflicts.is_empty() {
            println!(
                "shortcut.{}={} current={} default={}",
                diagnostic.label,
                diagnostic.state.as_str(),
                current,
                defaults
            );
        } else {
            println!(
                "shortcut.{}={} current={} default={} conflicts={}",
                diagnostic.label,
                diagnostic.state.as_str(),
                current,
                defaults,
                diagnostic.conflicts.join(",")
            );
        }
    }
}

fn kglobalaccel_diagnostics() -> zbus::Result<Vec<ShortcutDiagnostic>> {
    let connection = Connection::session()?;
    let root = Proxy::new(
        &connection,
        KGLOBALACCEL_NAME,
        KGLOBALACCEL_PATH,
        KGLOBALACCEL_INTERFACE,
    )?;
    let components: Vec<OwnedObjectPath> = root.call("allComponents", &())?;
    EXPECTED_SHORTCUTS
        .iter()
        .map(|expected| diagnose_shortcut(&connection, &root, &components, *expected))
        .collect()
}

fn diagnose_shortcut(
    connection: &Connection,
    root: &Proxy<'_>,
    components: &[OwnedObjectPath],
    expected: ExpectedShortcut,
) -> zbus::Result<ShortcutDiagnostic> {
    if !components
        .iter()
        .any(|path| path.as_str() == expected.component_path)
    {
        return Ok(ShortcutDiagnostic {
            label: expected.label,
            state: ShortcutState::MissingComponent,
            current: Vec::new(),
            defaults: Vec::new(),
            conflicts: Vec::new(),
        });
    }
    let component = Proxy::new(
        connection,
        KGLOBALACCEL_NAME,
        expected.component_path,
        KGLOBALACCEL_COMPONENT_INTERFACE,
    )?;
    let active: bool = component.call("isActive", &())?;
    let infos: Vec<KGlobalShortcutInfo> = component.call("allShortcutInfos", &())?;
    let Some(info) = infos.iter().find(|info| info.0 == expected.action_id) else {
        return Ok(ShortcutDiagnostic {
            label: expected.label,
            state: ShortcutState::MissingAction,
            current: Vec::new(),
            defaults: Vec::new(),
            conflicts: Vec::new(),
        });
    };
    let current = bound_shortcuts(&info.6);
    let defaults = bound_shortcuts(&info.7);
    let inspected = if current.is_empty() {
        &defaults
    } else {
        &current
    };
    let mut conflicts = Vec::new();
    for key in inspected {
        let owners: Vec<KGlobalShortcutInfo> = root.call("getGlobalShortcutsByKey", key)?;
        conflicts.extend(other_shortcut_owners(
            &owners,
            expected.component_id,
            expected.action_id,
            *key,
        ));
    }
    conflicts.sort();
    conflicts.dedup();
    let state = classify_shortcut(active, &current, &conflicts);
    Ok(ShortcutDiagnostic {
        label: expected.label,
        state,
        current,
        defaults,
        conflicts,
    })
}

fn bound_shortcuts(shortcuts: &[i32]) -> Vec<i32> {
    shortcuts
        .iter()
        .copied()
        .filter(|shortcut| *shortcut != 0)
        .collect()
}

fn classify_shortcut(active: bool, current: &[i32], conflicts: &[String]) -> ShortcutState {
    if !active {
        ShortcutState::Inactive
    } else if !conflicts.is_empty() {
        ShortcutState::Conflict
    } else if current.is_empty() {
        ShortcutState::Unbound
    } else {
        ShortcutState::Ok
    }
}

fn other_shortcut_owners(
    owners: &[KGlobalShortcutInfo],
    component_id: &str,
    action_id: &str,
    key: i32,
) -> Vec<String> {
    owners
        .iter()
        .filter(|owner| owner.2 != component_id || owner.0 != action_id)
        .map(|owner| format!("{}:{}@{}", owner.2, owner.0, format_qt_shortcut(key)))
        .collect()
}

fn format_shortcuts(shortcuts: &[i32]) -> String {
    if shortcuts.is_empty() {
        return "none".to_owned();
    }
    shortcuts
        .iter()
        .map(|shortcut| format_qt_shortcut(*shortcut))
        .collect::<Vec<_>>()
        .join("|")
}

fn format_qt_shortcut(shortcut: i32) -> String {
    const SHIFT: u32 = 0x0200_0000;
    const CTRL: u32 = 0x0400_0000;
    const ALT: u32 = 0x0800_0000;
    const META: u32 = 0x1000_0000;
    const KEYPAD: u32 = 0x2000_0000;
    const MODIFIERS: u32 = SHIFT | CTRL | ALT | META | KEYPAD;

    let value = u32::from_ne_bytes(shortcut.to_ne_bytes());
    let mut parts = Vec::with_capacity(6);
    if value & META != 0 {
        parts.push("Meta".to_owned());
    }
    if value & CTRL != 0 {
        parts.push("Ctrl".to_owned());
    }
    if value & ALT != 0 {
        parts.push("Alt".to_owned());
    }
    if value & SHIFT != 0 {
        parts.push("Shift".to_owned());
    }
    if value & KEYPAD != 0 {
        parts.push("Keypad".to_owned());
    }
    let key = value & !MODIFIERS;
    parts.push(format_qt_key(key));
    parts.join("+")
}

fn format_qt_key(key: u32) -> String {
    match key {
        0x0100_0000 => "Escape".to_owned(),
        0x0100_0001 => "Tab".to_owned(),
        0x0100_0002 => "Backtab".to_owned(),
        0x0100_0003 => "Backspace".to_owned(),
        0x0100_0004 => "Return".to_owned(),
        0x0100_0005 => "Enter".to_owned(),
        0x0100_0006 => "Insert".to_owned(),
        0x0100_0007 => "Delete".to_owned(),
        0x0100_0010 => "Home".to_owned(),
        0x0100_0011 => "End".to_owned(),
        0x0100_0012 => "Left".to_owned(),
        0x0100_0013 => "Up".to_owned(),
        0x0100_0014 => "Right".to_owned(),
        0x0100_0015 => "Down".to_owned(),
        0x0100_0016 => "PageUp".to_owned(),
        0x0100_0017 => "PageDown".to_owned(),
        0x0100_0030..=0x0100_0052 => format!("F{}", key - 0x0100_0030 + 1),
        0x20 => "Space".to_owned(),
        value @ 0x21..=0x7e => char::from_u32(value).map_or_else(
            || format!("Key(0x{value:08X})"),
            |character| character.to_string(),
        ),
        value => format!("Key(0x{value:08X})"),
    }
}

fn config_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then_some(value.trim())
    })
}

fn user_service_state(service: &str) -> String {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", service])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(command).is_file())
}

fn config_command(action: &str) -> Result<(), Box<dyn Error>> {
    match action {
        "path" => println!("{}", config_path()?.display()),
        "validate" => {
            let config = Config::load_or_create()?;
            println!(
                "valid schema={} profiles={} providers={}",
                config.schema_version,
                config.profiles.len(),
                config.providers.len()
            );
        }
        unknown => return Err(format!("unknown config command: {unknown}").into()),
    }
    Ok(())
}

fn secret_command(action: &str, name: &str) -> Result<(), Box<dyn Error>> {
    if action != "set" || name.is_empty() {
        return Err("usage: voxtype secret set NAME (secret is read from stdin)".into());
    }
    let mut secret = Vec::new();
    io::stdin().read_to_end(&mut secret)?;
    while matches!(secret.last(), Some(b'\n' | b'\r')) {
        secret.pop();
    }
    if secret.is_empty() {
        return Err("refusing to store an empty secret".into());
    }
    store_secret(name, &secret)?;
    secret.fill(0);
    println!("stored secret reference: {name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut_info(action: &str, component: &str) -> KGlobalShortcutInfo {
        (
            action.to_owned(),
            String::new(),
            component.to_owned(),
            String::new(),
            "default".to_owned(),
            "Default Context".to_owned(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn formats_current_plasma_shortcuts() {
        assert_eq!(format_qt_shortcut(402_653_270), "Meta+Alt+V");
        assert_eq!(format_qt_shortcut(402_653_267), "Meta+Alt+S");
        assert_eq!(format_qt_shortcut(402_653_272), "Meta+Alt+X");
        assert_eq!(format_qt_shortcut(419_430_400), "Meta+Alt+Escape");
        assert_eq!(format_qt_shortcut(402_653_255), "Meta+Alt+G");
    }

    #[test]
    fn diagnoses_every_packaged_voice_action() {
        let labels = EXPECTED_SHORTCUTS
            .iter()
            .map(|shortcut| shortcut.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["toggle", "cancel", "start", "stop", "grammar"]);
    }

    #[test]
    fn formats_alternative_and_unbound_shortcuts() {
        assert_eq!(format_shortcuts(&[]), "none");
        assert_eq!(
            format_shortcuts(&[402_653_270, 0x0400_0044]),
            "Meta+Alt+V|Ctrl+D"
        );
    }

    #[test]
    fn excludes_the_expected_owner_from_conflicts() {
        let owners = vec![
            shortcut_info("_launch", "io.github.tinnci.VoxType.desktop"),
            shortcut_info("other", "org.example.Other.desktop"),
        ];
        let conflicts = other_shortcut_owners(
            &owners,
            "io.github.tinnci.VoxType.desktop",
            "_launch",
            402_653_270,
        );
        assert_eq!(conflicts, ["org.example.Other.desktop:other@Meta+Alt+V"]);
    }

    #[test]
    fn classifies_inactive_conflict_and_unbound_states() {
        assert_eq!(classify_shortcut(false, &[1], &[]), ShortcutState::Inactive);
        assert_eq!(
            classify_shortcut(true, &[], &["owner".to_owned()]),
            ShortcutState::Conflict
        );
        assert_eq!(classify_shortcut(true, &[], &[]), ShortcutState::Unbound);
        assert_eq!(classify_shortcut(true, &[1], &[]), ShortcutState::Ok);
    }

    #[test]
    fn classifies_audio_peak_for_actionable_diagnostics() {
        assert_eq!(microphone_level_status(100), "too-quiet");
        assert_eq!(microphone_level_status(2_000), "ok");
        assert_eq!(microphone_level_status(32_000), "clipping");
    }
}
