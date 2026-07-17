use std::collections::HashMap;
use std::error::Error;
use std::sync::{
    Arc, RwLock,
    mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
};
use std::thread;
use std::time::Duration;
use voxtype::client::Client;
use zbus::blocking::{Connection, Proxy, connection::Builder};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

const TRAY_NAME: &str = "io.github.tinnci.VoxType.Tray";
const TRAY_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
type MenuProperties = HashMap<String, OwnedValue>;
type MenuNode = (i32, MenuProperties, Vec<OwnedValue>);
type MenuLayout = (u32, i32, MenuProperties, Vec<MenuNode>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrayPresentation {
    daemon_state: String,
    status: &'static str,
    icon_name: &'static str,
}

impl TrayPresentation {
    fn from_daemon_status(status: &str) -> Self {
        match status {
            "preparing" => Self::active(status, "microphone-sensitivity-medium"),
            "listening" => Self::attention(status, "microphone-sensitivity-high"),
            "finalizing" => Self::active(status, "content-loading"),
            "inserting" => Self::active(status, "insert-text"),
            "completed" => Self::active(status, "emblem-default"),
            "cancelled" => Self::active(status, "process-stop"),
            "failed" | "unavailable" => Self::attention(status, "dialog-error"),
            _ => Self::active(status, "audio-input-microphone"),
        }
    }

    fn active(daemon_state: &str, icon_name: &'static str) -> Self {
        Self {
            daemon_state: daemon_state.to_owned(),
            status: "Active",
            icon_name,
        }
    }

    fn attention(daemon_state: &str, icon_name: &'static str) -> Self {
        Self {
            daemon_state: daemon_state.to_owned(),
            status: "NeedsAttention",
            icon_name,
        }
    }

    fn is_busy(&self) -> bool {
        matches!(
            self.daemon_state.as_str(),
            "preparing" | "listening" | "finalizing" | "inserting"
        )
    }
}

#[derive(Clone)]
struct TrayItem {
    presentation: Arc<RwLock<TrayPresentation>>,
}

impl TrayItem {
    fn presentation(&self) -> TrayPresentation {
        self.presentation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

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
    fn status(&self) -> String {
        self.presentation().status.to_owned()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.presentation().icon_name.to_owned()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }

    #[zbus(property, name = "Menu")]
    fn menu(&self) -> &str {
        MENU_PATH
    }

    fn activate(&self, x: i32, y: i32) {
        let _ = (x, y);
        run_action("Toggle dictation", |client| client.toggle(""));
    }

    fn secondary_activate(&self, x: i32, y: i32) {
        let _ = (x, y);
        run_action("Cancel dictation", |client| client.cancel(""));
    }

    fn context_menu(&self, x: i32, y: i32) {
        let _ = (x, y);
    }

    #[zbus(signal, name = "NewStatus")]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "NewIcon")]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[derive(Clone)]
struct TrayMenu {
    presentation: Arc<RwLock<TrayPresentation>>,
}

impl TrayMenu {
    fn presentation(&self) -> TrayPresentation {
        self.presentation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
#[allow(clippy::unused_self)]
impl TrayMenu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> MenuLayout {
        let _ = (recursion_depth, property_names);
        (
            1,
            parent_id,
            HashMap::new(),
            menu_children(&self.presentation()),
        )
    }

    fn about_to_show(&self, id: i32) -> bool {
        let _ = id;
        false
    }

    fn event(&self, id: i32, event_id: &str, data: OwnedValue, timestamp: u32) {
        let _ = (data, timestamp);
        if event_id != "clicked" {
            return;
        }
        if !menu_item_enabled(id, &self.presentation()) {
            return;
        }
        match id {
            1 => {
                run_action("Start dictation", |client| client.start(""));
            }
            2 => {
                run_action("Stop dictation", |client| client.stop(""));
            }
            3 => {
                run_action("Cancel dictation", |client| client.cancel(""));
            }
            4 => {
                #[allow(clippy::redundant_closure_for_method_calls)]
                run_action("Local text cleanup", |client| client.check_last_grammar());
            }
            5 =>
            {
                #[allow(clippy::redundant_closure_for_method_calls)]
                if let Ok(status) = with_client(|client| client.usage_status()) {
                    let summary = usage_summary(&status);
                    let _ = std::process::Command::new("notify-send")
                        .args(["--app-name=VoxType", "VoxType usage", &summary])
                        .spawn();
                }
            }
            6 => {
                let _ = std::process::Command::new("voxtype-settings").spawn();
            }
            7 => {
                #[allow(clippy::redundant_closure_for_method_calls)]
                if let Ok(status) = with_client(|client| client.provider_status()) {
                    let _ = std::process::Command::new("notify-send")
                        .args(["--app-name=VoxType", "VoxType Provider", &status])
                        .spawn();
                }
                let _ = std::process::Command::new("notify-send")
                    .args([
                        "--app-name=VoxType",
                        "VoxType diagnostics",
                        "Run: voxtype doctor",
                    ])
                    .spawn();
            }
            8 => {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "stop", "voxtype-tray.service"])
                    .spawn();
            }
            _ => {}
        }
    }

    #[zbus(signal, name = "LayoutUpdated")]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

fn menu_children(presentation: &TrayPresentation) -> Vec<MenuNode> {
    #[allow(clippy::redundant_closure_for_method_calls)]
    let usage_label = with_client(|client| client.usage_status()).map_or_else(
        |_| "用量：不可用".to_owned(),
        |status| usage_summary(&status),
    );
    [
        (1, "开始语音输入".to_owned()),
        (2, "停止语音输入".to_owned()),
        (3, "取消当前任务".to_owned()),
        (4, "整理最近输入".to_owned()),
        (5, usage_label),
        (6, "设置与 API 密钥".to_owned()),
        (7, "诊断状态".to_owned()),
        (8, "退出托盘".to_owned()),
    ]
    .into_iter()
    .map(|(id, label)| {
        let mut properties = HashMap::new();
        properties.insert(
            "label".to_owned(),
            OwnedValue::try_from(Value::from(label))
                .expect("static menu labels are valid D-Bus values"),
        );
        properties.insert(
            "enabled".to_owned(),
            OwnedValue::try_from(Value::from(menu_item_enabled(id, presentation)))
                .expect("menu enabled state is a valid D-Bus value"),
        );
        (id, properties, Vec::new())
    })
    .collect()
}

fn menu_item_enabled(id: i32, presentation: &TrayPresentation) -> bool {
    match id {
        1 => presentation.daemon_state != "unavailable" && !presentation.is_busy(),
        2 => presentation.daemon_state == "listening",
        3 => presentation.is_busy(),
        _ => true,
    }
}

fn usage_summary(status: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(status) else {
        return "用量：数据不可用".to_owned();
    };
    let Some(providers) = value
        .get("providers")
        .and_then(serde_json::Value::as_object)
    else {
        return "用量：数据不可用".to_owned();
    };
    let summaries = providers
        .iter()
        .map(|(id, entry)| {
            let usage = &entry["usage"];
            let requests = usage["requests"].as_u64().unwrap_or(0);
            let audio_millis = usage["audio_millis"].as_u64().unwrap_or(0);
            let token_reports = usage["token_reports"].as_u64().unwrap_or(0);
            let tokens = usage["reported_tokens"].as_u64().unwrap_or(0);
            if token_reports > 0 {
                format!(
                    "{id}: {requests} 次 · {} · {tokens} token",
                    format_audio_duration(audio_millis)
                )
            } else {
                format!(
                    "{id}: {requests} 次 · {} · token 未报告",
                    format_audio_duration(audio_millis)
                )
            }
        })
        .collect::<Vec<_>>();
    if summaries.is_empty() {
        "用量：无 Provider".to_owned()
    } else {
        format!("用量（本次 daemon 会话）：{}", summaries.join("；"))
    }
}

fn format_audio_duration(millis: u64) -> String {
    format!("{}.{:01}s", millis / 1_000, (millis % 1_000) / 100)
}

fn with_client<T>(operation: impl FnOnce(&Client<'_>) -> zbus::Result<T>) -> zbus::Result<T> {
    let connection = Connection::session()?;
    let client = Client::connect(&connection)?;
    operation(&client)
}

fn run_action<T>(name: &str, operation: impl FnOnce(&Client<'_>) -> zbus::Result<T>) {
    if let Err(error) = with_client(operation) {
        let message = format!("{name} failed: {error}");
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=VoxType", "VoxType", &message])
            .spawn();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let initial = read_presentation();
    let presentation = Arc::new(RwLock::new(initial));
    let connection = Builder::session()?
        .name(TRAY_NAME)?
        .serve_at(
            TRAY_PATH,
            TrayItem {
                presentation: Arc::clone(&presentation),
            },
        )?
        .serve_at(
            MENU_PATH,
            TrayMenu {
                presentation: Arc::clone(&presentation),
            },
        )?
        .build()?;
    let watcher = Proxy::new(
        &connection,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    watcher.call_method("RegisterStatusNotifierItem", &(TRAY_NAME))?;
    let state_updates = spawn_state_listener()?;
    loop {
        let next = match state_updates.recv_timeout(Duration::from_secs(5)) {
            Ok(state) => TrayPresentation::from_daemon_status(&state),
            Err(RecvTimeoutError::Timeout) => read_presentation(),
            Err(RecvTimeoutError::Disconnected) => {
                TrayPresentation::from_daemon_status("unavailable")
            }
        };
        let previous = {
            let mut current = presentation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *current == next {
                continue;
            }
            std::mem::replace(&mut *current, next.clone())
        };
        emit_presentation_change(&connection, &presentation, &previous, &next)?;
    }
}

fn spawn_state_listener() -> std::io::Result<Receiver<String>> {
    let (sender, receiver) = sync_channel(16);
    thread::Builder::new()
        .name("voxtype-tray-state-listener".to_owned())
        .spawn(move || listen_for_daemon_states(&sender))?;
    Ok(receiver)
}

fn listen_for_daemon_states(sender: &SyncSender<String>) {
    loop {
        match watch_daemon_once(sender) {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                if sender.send("unavailable".to_owned()).is_err() {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn watch_daemon_once(sender: &SyncSender<String>) -> zbus::Result<bool> {
    let connection = Connection::session()?;
    let proxy = Proxy::new(
        &connection,
        voxtype::DBUS_NAME,
        voxtype::DBUS_PATH,
        voxtype::DBUS_INTERFACE,
    )?;
    let initial: String = proxy.call("Status", &())?;
    if sender.send(initial).is_err() {
        return Ok(false);
    }
    let mut signals = proxy.receive_signal("StateChanged")?;
    for message in &mut signals {
        let (state, _session): (String, String) = message.body().deserialize()?;
        if sender.send(state).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_presentation() -> TrayPresentation {
    #[allow(clippy::redundant_closure_for_method_calls)]
    with_client(|client| client.status()).map_or_else(
        |_| TrayPresentation::from_daemon_status("unavailable"),
        |status| TrayPresentation::from_daemon_status(&status),
    )
}

fn emit_presentation_change(
    connection: &Connection,
    presentation: &Arc<RwLock<TrayPresentation>>,
    previous: &TrayPresentation,
    next: &TrayPresentation,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(connection.inner(), TRAY_PATH)?;
    let item = TrayItem {
        presentation: Arc::clone(presentation),
    };
    zbus::block_on(async {
        if previous.status != next.status {
            item.status_changed(&emitter).await?;
            TrayItem::new_status(&emitter, next.status).await?;
        }
        if previous.icon_name != next.icon_name {
            item.icon_name_changed(&emitter).await?;
            TrayItem::new_icon(&emitter).await?;
        }
        Ok::<(), zbus::Error>(())
    })?;
    let menu_emitter = SignalEmitter::new(connection.inner(), MENU_PATH)?;
    zbus::block_on(TrayMenu::layout_updated(&menu_emitter, 1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_user_visible_phase_to_a_distinct_presentation() {
        let idle = TrayPresentation::from_daemon_status("idle");
        let listening = TrayPresentation::from_daemon_status("listening");
        let processing = TrayPresentation::from_daemon_status("finalizing");
        let inserting = TrayPresentation::from_daemon_status("inserting");
        let completed = TrayPresentation::from_daemon_status("completed");
        let cancelled = TrayPresentation::from_daemon_status("cancelled");
        let failed = TrayPresentation::from_daemon_status("failed");

        assert_eq!(idle.status, "Active");
        assert_eq!(listening.status, "NeedsAttention");
        assert_ne!(idle.icon_name, listening.icon_name);
        assert_ne!(listening.icon_name, processing.icon_name);
        assert_ne!(processing.icon_name, inserting.icon_name);
        assert_eq!(completed.icon_name, "emblem-default");
        assert_eq!(cancelled.icon_name, "process-stop");
        assert_eq!(failed.status, "NeedsAttention");
    }

    #[test]
    fn unavailable_daemon_is_visible_as_attention() {
        let state = TrayPresentation::from_daemon_status("unavailable");
        assert_eq!(state.status, "NeedsAttention");
        assert_eq!(state.icon_name, "dialog-error");
    }

    #[test]
    fn menu_actions_follow_daemon_state() {
        let idle = TrayPresentation::from_daemon_status("idle");
        assert!(menu_item_enabled(1, &idle));
        assert!(!menu_item_enabled(2, &idle));
        assert!(!menu_item_enabled(3, &idle));

        let listening = TrayPresentation::from_daemon_status("listening");
        assert!(!menu_item_enabled(1, &listening));
        assert!(menu_item_enabled(2, &listening));
        assert!(menu_item_enabled(3, &listening));

        let processing = TrayPresentation::from_daemon_status("finalizing");
        assert!(!menu_item_enabled(1, &processing));
        assert!(!menu_item_enabled(2, &processing));
        assert!(menu_item_enabled(3, &processing));

        let unavailable = TrayPresentation::from_daemon_status("unavailable");
        assert!(!menu_item_enabled(1, &unavailable));
    }

    #[test]
    fn summarizes_usage_without_inventing_tokens() {
        let status = r#"{"providers":{"cloud":{"usage":{"requests":2,"audio_millis":1500,"token_reports":0,"reported_tokens":0}}}}"#;
        let summary = usage_summary(status);
        assert!(summary.contains("cloud: 2 次 · 1.5s"));
        assert!(summary.contains("token 未报告"));
    }
}
