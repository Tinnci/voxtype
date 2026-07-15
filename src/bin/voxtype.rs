use std::env;
use std::error::Error;
use std::io::{self, Read};
use voxtype::client::Client;
use voxtype::config::{Config, config_path, store_secret};
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

    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    match command.as_str() {
        "status" => println!("{}", client.status()?),
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
        "VoxType CLI\n\nUsage:\n  voxtype status\n  voxtype start [PROFILE]\n  voxtype stop [SESSION]\n  voxtype toggle [PROFILE]\n  voxtype cancel [SESSION]\n  voxtype reset\n  voxtype reload\n  voxtype insert-test TEXT\n  voxtype config path|validate\n  voxtype secret set NAME"
    );
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
