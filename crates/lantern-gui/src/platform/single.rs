use super::{UiEvent, UiSender};
use lantern_cli::args::GuiArgs;
use std::collections::HashMap;
use zbus::{Connection, fdo::RequestNameFlags, zvariant::OwnedValue};

const PATH: &str = "/dev/lantern/Lantern";
const INTERFACE: &str = "org.freedesktop.Application";
const DEFAULT_NAME: &str = "dev.lantern.Lantern";
type PlatformData = HashMap<String, OwnedValue>;

#[derive(Debug)]
struct Application {
    sender: UiSender,
}
#[zbus::interface(name = "org.freedesktop.Application")]
impl Application {
    fn activate(&self, _platform_data: PlatformData) {
        let _ = self.sender.send(UiEvent::ShowWindow(None));
    }
    fn open(&self, uris: Vec<String>, _platform_data: PlatformData) {
        for uri in uris {
            if let Some(id) = uri.strip_prefix("lantern://instance/") {
                let _ = self.sender.send(UiEvent::ShowTarget(id.to_string()));
            }
        }
    }
    fn activate_action(&self, action_name: &str, _parameter: Vec<OwnedValue>, _platform_data: PlatformData) {
        if action_name == "quit" {
            let _ = self.sender.send(UiEvent::Quit);
        }
    }
}

pub(crate) enum Acquired {
    Primary(Connection),
    Secondary,
}

pub(crate) async fn acquire(name: &str, sender: UiSender, args: &GuiArgs) -> zbus::Result<Acquired> {
    let conn = zbus::connection::Builder::session()?
        .serve_at(PATH, Application { sender })?
        .build()
        .await?;
    match conn
        .request_name_with_flags(name.to_owned(), RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(_) if args.quit => Ok(Acquired::Secondary),
        Ok(_) => Ok(Acquired::Primary(conn)),
        Err(zbus::Error::NameTaken) => {
            let data = PlatformData::new();
            if args.quit {
                conn.call_method(
                    Some(name),
                    PATH,
                    Some(INTERFACE),
                    "ActivateAction",
                    &("quit", Vec::<OwnedValue>::new(), data),
                )
                .await?;
            } else if let Some(target) = &args.target {
                conn.call_method(
                    Some(name),
                    PATH,
                    Some(INTERFACE),
                    "Open",
                    &(vec![format!("lantern://instance/{target}")], data),
                )
                .await?;
            } else if !args.tray {
                conn.call_method(Some(name), PATH, Some(INTERFACE), "Activate", &(data,))
                    .await?;
            }
            Ok(Acquired::Secondary)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn bus_name() -> String {
    std::env::var("LANTERN_GUI_BUS_NAME").unwrap_or_else(|_| DEFAULT_NAME.to_string())
}
