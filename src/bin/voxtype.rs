use std::env;
use std::error::Error;
use std::io::{self, Read};
use voxtype::client::Client;
use voxtype::config::{Config, config_path, store_secret};
use voxtype::fcitx::FcitxBridge;
use zbus::blocking::Connection;

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
        return doctor_command();
    }

    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    match command.as_str() {
        "status" => println!("{}", client.status()?),
        "providers" => println!("{}", client.provider_status()?),
        "fcitx-focus" => {
            if client.status()? != "idle" {
                return Err("fcitx focus probing requires an idle daemon".into());
            }
            let target = FcitxBridge.probe()?;
            println!("program={} frontend={}", target.program, target.frontend);
        }
        "start" => println!(
            "{}",
            client.start(arguments.next().as_deref().unwrap_or(""))?
        ),
        "stop" => println!(
            "{}",
            client.stop(arguments.next().as_deref().unwrap_or(""))?
        ),
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
        "VoxType CLI\n\nUsage:\n  voxtype status\n  voxtype providers\n  voxtype fcitx-focus\n  voxtype start [PROFILE]\n  voxtype stop [SESSION]\n  voxtype toggle [PROFILE]\n  voxtype cancel [SESSION]\n  voxtype reset\n  voxtype reload\n  voxtype doctor\n  voxtype insert-test TEXT\n  voxtype config path|validate\n  voxtype secret set NAME"
    );
}

fn doctor_command() -> Result<(), Box<dyn Error>> {
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
    println!("shortcut.kglobalaccel={}", kglobalaccel_state());
    for command in [
        "parec",
        "curl",
        "wl-copy",
        "wl-paste",
        "ydotool",
        "notify-send",
        "qdbus6",
    ] {
        if command_exists(command) {
            println!("command.{command}=ok");
        } else {
            println!("command.{command}=missing");
        }
    }
    Ok(())
}

fn kglobalaccel_state() -> &'static str {
    let output = std::process::Command::new("qdbus6")
        .args([
            "org.kde.kglobalaccel",
            "/component/io_github_tinnci_VoxType_desktop",
            "org.kde.kglobalaccel.Component.shortcutNames",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let shortcuts = String::from_utf8_lossy(&output.stdout);
            if shortcuts.contains("_launch") && shortcuts.contains("Cancel") {
                "ok"
            } else {
                "unregistered"
            }
        }
        Ok(_) => "unregistered",
        Err(_) => "unavailable",
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
