use std::error::Error;
use std::thread;
use std::time::Duration;
use voxtype::{
    DBUS_NAME, DBUS_PATH,
    daemon::{DaemonEvent, VoxTypeDaemon},
};
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
        let mut daemon = interface.get_mut();
        if daemon.should_quit_value() {
            break;
        }
        daemon.poll_audio_telemetry();
        if let Err(error) = daemon.enforce_recording_deadline() {
            eprintln!("voxtyped deadline handling failed: {error}");
        }
        if let Err(error) = daemon.poll_recognition() {
            eprintln!("voxtyped recognition failed: {error}");
        }
        let events = daemon.drain_events();
        drop(daemon);
        for event in events {
            let result = match event {
                DaemonEvent::StateChanged { state, session } => zbus::block_on(
                    VoxTypeDaemon::state_changed(interface.signal_emitter(), &state, &session),
                ),
                DaemonEvent::SessionFinished {
                    session,
                    outcome,
                    error_code,
                    backend,
                    char_count,
                } => zbus::block_on(VoxTypeDaemon::session_finished(
                    interface.signal_emitter(),
                    &session,
                    &outcome,
                    &error_code,
                    &backend,
                    char_count,
                )),
            };
            if let Err(error) = result {
                eprintln!("voxtyped lifecycle signal failed: {error}");
            }
        }
        drop(interface);
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
