use super::{UiEvent, UiSender};
use crate::model::{Connection, Mirror};
use ksni::{
    TrayMethods,
    menu::{StandardItem, SubMenu},
};
use lantern_core::{ids::InstanceId, model::InstanceStatus};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TraySnapshot {
    pub online: bool,
    pub running: Vec<InstanceId>,
    pub attention: usize,
}
impl TraySnapshot {
    pub fn from_mirror(mirror: &Mirror) -> Self {
        let online = matches!(mirror.connection, Connection::Live);
        let instances = mirror.live.instances.values().filter(|i| {
            i.visible
                && mirror
                    .live
                    .projects
                    .get(&i.project)
                    .is_some_and(|p| p.project.enabled)
        });
        let mut running = Vec::new();
        let mut attention = 0;
        for instance in instances {
            match instance.runtime.status {
                InstanceStatus::Running | InstanceStatus::Starting => running.push(instance.id.clone()),
                InstanceStatus::Unhealthy | InstanceStatus::Crashed | InstanceStatus::NeedsConfig => {
                    attention += 1
                }
                _ => {}
            }
        }
        Self {
            online,
            running,
            attention,
        }
    }
    fn state(&self) -> IconState {
        if !self.online {
            IconState::Offline
        } else if self.attention > 0 {
            IconState::Attention
        } else if !self.running.is_empty() {
            IconState::Active
        } else {
            IconState::Idle
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconState {
    Idle,
    Active,
    Attention,
    Offline,
}
impl IconState {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "lantern-idle-symbolic",
            Self::Active => "lantern-active-symbolic",
            Self::Attention => "lantern-attention-symbolic",
            Self::Offline => "lantern-offline-symbolic",
        }
    }
    fn color(self) -> [u8; 4] {
        match self {
            Self::Idle => [255, 160, 160, 160],
            Self::Active => [255, 63, 185, 80],
            Self::Attention => [255, 248, 81, 73],
            Self::Offline => [255, 115, 115, 115],
        }
    }
}
#[derive(Debug)]
struct GuiTray {
    snapshot: TraySnapshot,
    sender: UiSender,
}
impl ksni::Tray for GuiTray {
    fn id(&self) -> String {
        "lantern".into()
    }
    fn title(&self) -> String {
        "Lantern".into()
    }
    fn status(&self) -> ksni::Status {
        if self.snapshot.state() == IconState::Attention {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn icon_name(&self) -> String {
        self.snapshot.state().name().into()
    }
    fn attention_icon_name(&self) -> String {
        IconState::Attention.name().into()
    }
    fn icon_theme_path(&self) -> String {
        std::env::var("LANTERN_ICON_DIR").unwrap_or_default()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let color = self.snapshot.state().color();
        let mut data = Vec::with_capacity(16 * 16 * 4);
        for y in 0..16 {
            for x in 0..16 {
                let inside = ((4..=11).contains(&x) && (4..=12).contains(&y))
                    || ((6..=9).contains(&x) && (2..=3).contains(&y))
                    || ((3..=12).contains(&x) && y == 13);
                data.extend_from_slice(if inside { &color } else { &[0, 0, 0, 0] });
            }
        }
        vec![ksni::Icon {
            width: 16,
            height: 16,
            data,
        }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Lantern".into(),
            description: if self.snapshot.online {
                format!(
                    "{} running, {} need attention",
                    self.snapshot.running.len(),
                    self.snapshot.attention
                )
            } else {
                "Daemon offline".into()
            },
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self
            .sender
            .send(UiEvent::ShowWindow(ksni::current_xdg_activation_token()));
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut menu = Vec::new();
        if !self.snapshot.online {
            menu.push(
                StandardItem {
                    label: "Daemon not running".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            menu.push(
                StandardItem {
                    label: "Start daemon".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.sender.send(UiEvent::StartDaemon);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        for id in self
            .snapshot
            .running
            .iter()
            .filter(|_| self.snapshot.online)
            .take(12)
        {
            let id = id.clone();
            let label = id.to_string();
            menu.push(
                SubMenu {
                    label,
                    submenu: vec![
                        StandardItem {
                            label: "Stop".into(),
                            activate: Box::new(move |tray: &mut Self| {
                                let _ = tray.sender.send(UiEvent::Stop(id.clone()));
                            }),
                            ..Default::default()
                        }
                        .into(),
                    ],
                    ..Default::default()
                }
                .into(),
            );
        }
        if self.snapshot.online && self.snapshot.running.len() > 12 {
            menu.push(
                StandardItem {
                    label: format!("{} more…", self.snapshot.running.len() - 12),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.sender.send(UiEvent::ShowWindow(None));
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu.push(ksni::MenuItem::Separator);
        menu.push(
            StandardItem {
                label: "Open Lantern".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.sender.send(UiEvent::ShowWindow(None));
                }),
                ..Default::default()
            }
            .into(),
        );
        menu.push(
            StandardItem {
                label: "Quit Lantern".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.sender.send(UiEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        menu
    }
}

pub(crate) async fn run(sender: UiSender, mut snapshots: watch::Receiver<TraySnapshot>) {
    let started = tokio::time::Instant::now();
    let handle = loop {
        let tray = GuiTray {
            sender: sender.clone(),
            snapshot: snapshots.borrow().clone(),
        };
        match tray.spawn().await {
            Ok(handle) => break handle,
            Err(error) if started.elapsed() < Duration::from_secs(60) => {
                eprintln!("tray registration failed, retrying: {error}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(error) => {
                eprintln!("tray unavailable: {error}");
                let _ = sender.send(UiEvent::TrayAvailable(false));
                return;
            }
        }
    };
    let _ = sender.send(UiEvent::TrayAvailable(true));
    while snapshots.changed().await.is_ok() {
        let snapshot = snapshots.borrow_and_update().clone();
        if handle.update(|tray| tray.snapshot = snapshot).await.is_none() {
            break;
        }
    }
    handle.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn icon_states_follow_connection_and_health() {
        let mut snapshot = TraySnapshot::default();
        assert_eq!(snapshot.state(), IconState::Offline);
        snapshot.online = true;
        assert_eq!(snapshot.state(), IconState::Idle);
        snapshot.running.push(InstanceId::parse("example").unwrap());
        assert_eq!(snapshot.state(), IconState::Active);
        snapshot.attention = 1;
        assert_eq!(snapshot.state(), IconState::Attention);
    }
}

#[cfg(test)]
mod bus_test {
    use super::*;
    use crate::platform::single::{Acquired, acquire};
    use lantern_cli::args::GuiArgs;
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct Watcher(Arc<Mutex<Vec<String>>>);
    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: &str) {
            self.0.lock().unwrap().push(service.to_owned());
        }
        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }
    }

    #[test]
    #[ignore = "real system: session bus"]
    fn single_instance_and_tray_on_private_bus() {
        assert_eq!(
            std::env::var("LANTERN_GUI_TEST_PRIVATE_BUS").as_deref(),
            Ok("1"),
            "run only under a private dbus-run-session"
        );
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(8), async {
                let items = Arc::new(Mutex::new(Vec::new()));
                let _watcher = zbus::connection::Builder::session()
                    .unwrap()
                    .name("org.kde.StatusNotifierWatcher")
                    .unwrap()
                    .serve_at("/StatusNotifierWatcher", Watcher(items.clone()))
                    .unwrap()
                    .build()
                    .await
                    .unwrap();
                let name = format!("dev.lantern.LanternTest{}", std::process::id());
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let primary = match acquire(&name, tx.clone(), &GuiArgs::default()).await.unwrap() {
                    Acquired::Primary(conn) => conn,
                    Acquired::Secondary => panic!("first instance was secondary"),
                };
                assert!(matches!(
                    acquire(&name, tx.clone(), &GuiArgs::default()).await.unwrap(),
                    Acquired::Secondary
                ));
                assert!(matches!(rx.recv().await, Some(UiEvent::ShowWindow(_))));
                let tray = GuiTray {
                    snapshot: TraySnapshot::default(),
                    sender: tx,
                }
                .spawn()
                .await
                .unwrap();
                let service = items.lock().unwrap().first().unwrap().clone();
                let item = zbus::Proxy::new(
                    &primary,
                    service,
                    "/StatusNotifierItem",
                    "org.kde.StatusNotifierItem",
                )
                .await
                .unwrap();
                let id: String = item.get_property("Id").await.unwrap();
                assert_eq!(id, "lantern");
                let dbus = zbus::fdo::DBusProxy::new(&primary).await.unwrap();
                assert!(
                    dbus.name_has_owner(name.as_str().try_into().unwrap())
                        .await
                        .unwrap()
                );
                tray.shutdown().await;
                drop(primary);
            })
            .await
            .expect("private bus test timed out");
        });
    }
}
