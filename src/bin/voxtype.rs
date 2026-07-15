use std::env;
use std::error::Error;
use voxtype::client::Client;
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
        unknown => return Err(format!("unknown command: {unknown}").into()),
    }
    Ok(())
}

fn print_help() {
    println!(
        "VoxType CLI\n\nUsage:\n  voxtype status\n  voxtype start [PROFILE]\n  voxtype stop [SESSION]\n  voxtype toggle [PROFILE]\n  voxtype cancel [SESSION]\n  voxtype reset\n  voxtype insert-test TEXT"
    );
}
