use lantern_core::ids::InstanceId;
use tokio::sync::mpsc;

pub(crate) mod single;
pub(crate) mod tray;

#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    ShowWindow(Option<String>),
    ShowTarget(String),
    Stop(InstanceId),
    StartDaemon,
    TrayAvailable(bool),
    Quit,
}
pub(crate) type UiSender = mpsc::UnboundedSender<UiEvent>;
pub(crate) type UiReceiver = mpsc::UnboundedReceiver<UiEvent>;
