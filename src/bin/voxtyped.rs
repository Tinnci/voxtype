use std::error::Error;
use std::thread;
use std::time::Duration;
use voxtype::{DBUS_NAME, DBUS_PATH, daemon::VoxTypeDaemon};
use zbus::blocking::connection::Builder;

fn main() -> Result<(), Box<dyn Error>> {
    let connection = Builder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, VoxTypeDaemon::load()?)?
        .build()?;

    eprintln!("voxtyped ready on {DBUS_NAME}");
    loop {
        let interface = connection
            .object_server()
            .interface::<_, VoxTypeDaemon>(DBUS_PATH)?;
        if interface.get().should_quit_value() {
            break;
        }
        drop(interface);
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}
