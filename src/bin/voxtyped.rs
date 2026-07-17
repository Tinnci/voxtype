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
    let mut last_snapshot = None;
    loop {
        let interface = connection
            .object_server()
            .interface::<_, VoxTypeDaemon>(DBUS_PATH)?;
        let mut daemon = interface.get_mut();
        if daemon.should_quit_value() {
            break;
        }
        if let Err(error) = daemon.enforce_recording_deadline() {
            eprintln!("voxtyped deadline handling failed: {error}");
        }
        if let Err(error) = daemon.poll_recognition() {
            eprintln!("voxtyped recognition failed: {error}");
        }
        let snapshot = daemon.state_snapshot();
        drop(daemon);
        if last_snapshot.as_ref() != Some(&snapshot) {
            if let Err(error) = zbus::block_on(VoxTypeDaemon::state_changed(
                interface.signal_emitter(),
                &snapshot.0,
                &snapshot.1,
            )) {
                eprintln!("voxtyped state signal failed: {error}");
            }
            last_snapshot = Some(snapshot);
        }
        drop(interface);
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
