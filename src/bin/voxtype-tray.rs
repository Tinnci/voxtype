use std::error::Error;
use std::thread;
use std::time::Duration;
use voxtype::client::Client;
use zbus::blocking::{Connection, Proxy, connection::Builder};

const TRAY_NAME: &str = "io.github.tinnci.VoxType.Tray";
const TRAY_PATH: &str = "/StatusNotifierItem";

struct TrayItem;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
#[allow(
    clippy::unnecessary_literal_bound,
    clippy::unused_self,
    clippy::used_underscore_binding
)]
impl TrayItem {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "voxtype"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "VoxType Voice Input"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "audio-input-microphone"
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    fn activate(&self, x: i32, y: i32) {
        let _ = (x, y);
        let _ = with_client(|client| client.toggle(""));
    }

    fn secondary_activate(&self, x: i32, y: i32) {
        let _ = (x, y);
        let _ = with_client(|client| client.cancel(""));
    }

    fn context_menu(&self, x: i32, y: i32) {
        let _ = (x, y);
    }
}

fn with_client<T>(operation: impl FnOnce(&Client<'_>) -> zbus::Result<T>) -> zbus::Result<T> {
    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    operation(&client)
}

fn main() -> Result<(), Box<dyn Error>> {
    let connection = Builder::session()?
        .name(TRAY_NAME)?
        .serve_at(TRAY_PATH, TrayItem)?
        .build()?;
    let watcher = Proxy::new(
        &connection,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    watcher.call_method("RegisterStatusNotifierItem", &(TRAY_NAME))?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
