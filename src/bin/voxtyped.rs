use std::error::Error;
use voxtype::{
    DBUS_NAME, DBUS_PATH,
    daemon::{DaemonEvent, VoxTypeDaemon},
};
use voxtype_app::wake_channel;
use zbus::blocking::connection::Builder;

fn main() -> Result<(), Box<dyn Error>> {
    let (wake, wake_receiver) = wake_channel();
    let daemon = VoxTypeDaemon::load_with_wake(wake)?;
    let connection = Builder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, daemon)?
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
        let maintenance_wait = daemon.maintenance_wait();
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
        let _result = wake_receiver.recv_timeout(maintenance_wait);
    }
    Ok(())
}
