use std::collections::HashMap;
use std::error::Error;
use std::thread;
use std::time::Duration;
use voxtype::client::Client;
use zbus::blocking::{Connection, Proxy, connection::Builder};
use zbus::zvariant::{OwnedValue, Value};

const TRAY_NAME: &str = "io.github.tinnci.VoxType.Tray";
const TRAY_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
type MenuProperties = HashMap<String, OwnedValue>;
type MenuNode = (i32, MenuProperties, Vec<OwnedValue>);
type MenuLayout = (u32, i32, MenuProperties, Vec<MenuNode>);

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
    fn status(&self) -> String {
        current_status().map_or_else(
            |_| "NeedsAttention".to_owned(),
            |active| {
                if active {
                    "NeedsAttention".to_owned()
                } else {
                    "Active".to_owned()
                }
            },
        )
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        current_status().map_or_else(
            |_| "audio-input-microphone".to_owned(),
            |active| {
                if active {
                    "microphone-sensitivity-high".to_owned()
                } else {
                    "audio-input-microphone".to_owned()
                }
            },
        )
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
}

struct TrayMenu;

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
        (1, parent_id, HashMap::new(), menu_children())
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
                run_action("Grammar check", |client| client.check_last_grammar());
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
}

fn menu_children() -> Vec<MenuNode> {
    #[allow(clippy::redundant_closure_for_method_calls)]
    let usage_label = with_client(|client| client.usage_status()).map_or_else(
        |_| "用量：不可用".to_owned(),
        |status| usage_summary(&status),
    );
    [
        (1, "开始语音输入".to_owned()),
        (2, "停止语音输入".to_owned()),
        (3, "取消当前录音".to_owned()),
        (4, "检查最近输入的语法".to_owned()),
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
        (id, properties, Vec::new())
    })
    .collect()
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

fn current_status() -> zbus::Result<bool> {
    with_client(|client| client.status().map(|status| is_active_status(&status)))
}

fn is_active_status(status: &str) -> bool {
    matches!(
        status,
        "preparing" | "listening" | "finalizing" | "inserting"
    )
}

fn main() -> Result<(), Box<dyn Error>> {
    let connection = Builder::session()?
        .name(TRAY_NAME)?
        .serve_at(TRAY_PATH, TrayItem)?
        .serve_at(MENU_PATH, TrayMenu)?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_state_uses_active_tray_icon() {
        assert!(is_active_status("listening"));
        assert!(!is_active_status("idle"));
    }

    #[test]
    fn summarizes_usage_without_inventing_tokens() {
        let status = r#"{"providers":{"cloud":{"usage":{"requests":2,"audio_millis":1500,"token_reports":0,"reported_tokens":0}}}}"#;
        let summary = usage_summary(status);
        assert!(summary.contains("cloud: 2 次 · 1.5s"));
        assert!(summary.contains("token 未报告"));
    }
}
