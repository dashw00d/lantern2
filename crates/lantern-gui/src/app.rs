use crate::fonts::Fonts;
use crate::model::{Connection, Effect, Mirror};
use futures_util::StreamExt;
use iced::{
    Element, Font, Length, Subscription, Task, Theme, theme,
    widget::{button, column, container, row, scrollable, text, text_input},
    window,
};
use lantern_cli::args::GuiArgs;
use lantern_client::{Client, EventsOptions, StreamEvent, Wait};
use lantern_core::{
    api::{OperationId, OperationResponse, OperationStatus},
    ids::InstanceId,
    model::InstanceStatus,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub(crate) fn run(args: GuiArgs, fonts: Fonts, client: Client) -> iced::Result {
    iced::daemon(
        move || App::boot(&args, fonts, client.clone()),
        App::update,
        App::view,
    )
    .title("Lantern")
    .default_font(Font::with_name(fonts.sans))
    .font(lucide_icons::LUCIDE_FONT_BYTES)
    .theme(App::theme)
    .subscription(App::subscription)
    .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Start,
    Stop,
    Restart,
}
#[derive(Debug, Clone)]
struct Command {
    id: InstanceId,
    kind: CommandKind,
}
impl Command {
    async fn execute(self, client: &Client) -> Result<OperationResponse, String> {
        let result = match self.kind {
            CommandKind::Start => client.start(&self.id, Wait::default()).await,
            CommandKind::Stop => client.stop(&self.id, Wait::default()).await,
            CommandKind::Restart => client.restart(&self.id, Wait::default()).await,
        };
        result.map_err(|e| e.to_string())
    }
}
#[derive(Debug)]
struct Pending {
    kind: CommandKind,
    since: Instant,
    operation: Option<OperationId>,
    step: String,
}

#[derive(Debug)]
struct App {
    window: Option<window::Id>,
    mode: theme::Mode,
    fonts: Fonts,
    client: Client,
    mirror: Mirror,
    pending: BTreeMap<InstanceId, Pending>,
    toasts: Vec<String>,
    filter: String,
    why: Option<String>,
    stream_generation: u64,
}
#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    Close(window::Id),
    Theme(theme::Mode),
    Stream(Box<StreamEvent>),
    Command(Command),
    CommandDone(InstanceId, Result<OperationResponse, String>),
    Tick(Instant),
    Filter(String),
    Dismiss(usize),
    Retry,
    StartDaemon,
    ShowWhy,
    DaemonDone(Result<String, String>, bool),
    CopyCommand,
}
impl App {
    fn boot(args: &GuiArgs, fonts: Fonts, client: Client) -> (Self, Task<Message>) {
        let mut app = Self {
            window: None,
            mode: theme::Mode::Dark,
            fonts,
            client,
            mirror: Mirror::default(),
            pending: BTreeMap::new(),
            toasts: vec![],
            filter: String::new(),
            why: None,
            stream_generation: 0,
        };
        let open = if args.tray { Task::none() } else { app.open() };
        (
            app,
            Task::batch([open, iced::system::theme().map(Message::Theme)]),
        )
    }
    fn open(&mut self) -> Task<Message> {
        let (id, task) = window::open(window::Settings {
            size: iced::Size::new(1000.0, 700.0),
            exit_on_close_request: false,
            platform_specific: window::settings::PlatformSpecific {
                application_id: "dev.lantern.Lantern".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        self.window = Some(id);
        task.map(Message::WindowOpened)
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.window = Some(id);
                Task::none()
            }
            Message::Close(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                window::close(id)
            }
            Message::Stream(event) => {
                for effect in self.mirror.stream(*event) {
                    match effect {
                        Effect::Restarted => self.pending.clear(),
                        Effect::Resynced => {
                            // Reconcile pending requests against the completed snapshot.
                            self.pending.retain(|id, pending| {
                                self.mirror
                                    .live
                                    .instances
                                    .get(id)
                                    .is_none_or(|instance| !settled(pending.kind, instance.runtime.status))
                            });
                        }
                        Effect::Transition { id, status } => {
                            if self.pending.get(&id).is_some_and(|p| settled(p.kind, status)) {
                                self.pending.remove(&id);
                            }
                        }
                        Effect::Progress {
                            op,
                            instance,
                            step,
                            done,
                            error,
                        } => {
                            let matches = self
                                .pending
                                .get(&instance)
                                .is_some_and(|p| p.operation.as_ref().is_none_or(|known| known == &op));
                            if matches {
                                if done {
                                    self.pending.remove(&instance);
                                    if let Some(error) = error {
                                        self.toasts.push(error);
                                    }
                                } else if let Some(pending) = self.pending.get_mut(&instance) {
                                    pending.operation = Some(op);
                                    pending.step = step;
                                }
                            }
                        }
                        Effect::Removed(id) => {
                            self.pending.remove(&id);
                        }
                    }
                }
                Task::none()
            }
            Message::Command(cmd) => {
                if !matches!(self.mirror.connection, Connection::Live) || self.pending.contains_key(&cmd.id) {
                    return Task::none();
                }
                let id = cmd.id.clone();
                self.pending.insert(
                    id.clone(),
                    Pending {
                        kind: cmd.kind,
                        since: Instant::now(),
                        operation: None,
                        step: "Working…".into(),
                    },
                );
                let client = self.client.clone();
                Task::perform(async move { cmd.execute(&client).await }, move |result| {
                    Message::CommandDone(id.clone(), result)
                })
            }
            Message::CommandDone(id, result) => {
                match result {
                    Err(error) => {
                        self.pending.remove(&id);
                        self.toasts.push(error);
                    }
                    Ok(response) => {
                        if response.operation.noop
                            || matches!(
                                response.operation.status,
                                OperationStatus::Succeeded
                                    | OperationStatus::Failed
                                    | OperationStatus::Cancelled
                            )
                        {
                            self.pending.remove(&id);
                            if let Some(error) = response.operation.error {
                                self.toasts.push(error.message);
                            }
                        } else if let Some(pending) = self.pending.get_mut(&id) {
                            pending.operation = Some(response.operation.id);
                        }
                    }
                }
                Task::none()
            }
            Message::Tick(now) => {
                let timed_out: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, p)| now.duration_since(p.since) >= Duration::from_secs(180))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in timed_out {
                    self.pending.remove(&id);
                    self.toasts
                        .push(format!("Still no answer from the daemon for {id}"));
                }
                Task::none()
            }
            Message::Filter(value) => {
                self.filter = value;
                Task::none()
            }
            Message::Dismiss(index) => {
                if index < self.toasts.len() {
                    self.toasts.remove(index);
                }
                Task::none()
            }
            Message::Retry => {
                self.stream_generation = self.stream_generation.wrapping_add(1);
                Task::none()
            }
            Message::StartDaemon => Task::perform(daemon_action(false), |r| Message::DaemonDone(r, false)),
            Message::ShowWhy => Task::perform(daemon_action(true), |r| Message::DaemonDone(r, true)),
            Message::DaemonDone(result, show) => {
                match result {
                    Ok(output) if show => self.why = Some(output),
                    Ok(_) => self.stream_generation = self.stream_generation.wrapping_add(1),
                    Err(error) => self.toasts.push(error),
                };
                Task::none()
            }
            Message::CopyCommand => iced::clipboard::write("systemctl --user start lantern".to_string()),
            Message::Theme(mode) => {
                self.mode = mode;
                Task::none()
            }
        }
    }
    fn view(&self, _: window::Id) -> Element<'_, Message> {
        let mut body = column![text("Lantern").size(28)].spacing(12).padding(20);
        if let Connection::Down { error, retry_in } = &self.mirror.connection {
            let message = if self.mirror.ever_live {
                format!(
                    "Connection lost: {error}. Retrying in {}s; displayed data may be stale.",
                    retry_in.as_secs_f32().ceil()
                )
            } else {
                format!(
                    "The Lantern daemon isn't running. Dev servers keep running while it is down.\n{error}\nRetrying automatically in {}s.",
                    retry_in.as_secs_f32().ceil()
                )
            };
            body = body
                .push(text(message))
                .push(text("systemctl --user start lantern").font(Font::with_name(self.fonts.mono)))
                .push(
                    row![
                        button("Start daemon").on_press(Message::StartDaemon),
                        button("Show why").on_press(Message::ShowWhy),
                        button("Copy command").on_press(Message::CopyCommand),
                        button("Retry now").on_press(Message::Retry)
                    ]
                    .spacing(8),
                );
            if let Some(why) = &self.why {
                body = body.push(text(why));
            }
        } else if matches!(self.mirror.connection, Connection::Connecting) {
            body = body.push(text("Connecting to Lantern…"));
        }
        if self.mirror.ever_live {
            let instances = self
                .mirror
                .live
                .instances
                .values()
                .filter(|i| i.visible)
                .collect::<Vec<_>>();
            let running = instances
                .iter()
                .filter(|i| {
                    matches!(
                        i.runtime.status,
                        InstanceStatus::Running | InstanceStatus::Starting
                    )
                })
                .count();
            let attention = instances
                .iter()
                .filter(|i| {
                    matches!(
                        i.runtime.status,
                        InstanceStatus::Crashed | InstanceStatus::Unhealthy | InstanceStatus::NeedsConfig
                    )
                })
                .count();
            body = body
                .push(text(format!(
                    "{running} running  ·  {attention} needs attention  ·  {} listeners",
                    self.mirror.live.listeners.len()
                )))
                .push(text_input("Filter projects or instances", &self.filter).on_input(Message::Filter));
            let filter = self.filter.to_lowercase();
            let mut list = column![].spacing(8);
            for project in self.mirror.live.projects.values() {
                let mut children: Vec<_> = instances
                    .iter()
                    .copied()
                    .filter(|i| {
                        i.project == project.project.id
                            && (filter.is_empty()
                                || project.project.name.to_lowercase().contains(&filter)
                                || i.id.as_str().contains(&filter))
                    })
                    .collect();
                if children.is_empty()
                    && !filter.is_empty()
                    && !project.project.name.to_lowercase().contains(&filter)
                {
                    continue;
                }
                children.sort_by_key(|i| {
                    (
                        i.id != project.primary,
                        !matches!(
                            i.runtime.status,
                            InstanceStatus::Running | InstanceStatus::Starting
                        ),
                        i.id.clone(),
                    )
                });
                list = list.push(
                    text(format!(
                        "{}   {}",
                        project.project.name,
                        project.project.root.display()
                    ))
                    .size(20),
                );
                for instance in children {
                    let status = instance.runtime.status;
                    let name = instance.id.to_string();
                    let glyph = match status {
                        InstanceStatus::Running => "●",
                        InstanceStatus::Starting | InstanceStatus::Preparing | InstanceStatus::Stopping => {
                            "◐"
                        }
                        InstanceStatus::Unhealthy => "▲",
                        InstanceStatus::Crashed | InstanceStatus::NeedsConfig => "✖",
                        InstanceStatus::External => "◇",
                        InstanceStatus::Stopped => "○",
                    };
                    let branch = instance.checkout.branch.as_deref().unwrap_or("detached");
                    let port = instance
                        .runtime
                        .port
                        .map_or_else(|| "-".to_owned(), |p| format!(":{p}"));
                    let mut line = row![
                        text(format!("{glyph}  {name}   {branch}   {port}   {}", instance.url))
                            .width(Length::Fill)
                    ]
                    .spacing(8);
                    if let Some(pending) = self.pending.get(&instance.id) {
                        line = line.push(text(format!("◌ {}", pending.step)));
                    } else {
                        let can_send = matches!(self.mirror.connection, Connection::Live);
                        let (label, kind) = if matches!(
                            status,
                            InstanceStatus::Running | InstanceStatus::Starting | InstanceStatus::Unhealthy
                        ) {
                            ("Stop", CommandKind::Stop)
                        } else {
                            ("Start", CommandKind::Start)
                        };
                        let action = button(label);
                        let restart = button("Restart");
                        line = line.push(if can_send {
                            action.on_press(Message::Command(Command {
                                id: instance.id.clone(),
                                kind,
                            }))
                        } else {
                            action
                        });
                        line = line.push(if can_send {
                            restart.on_press(Message::Command(Command {
                                id: instance.id.clone(),
                                kind: CommandKind::Restart,
                            }))
                        } else {
                            restart
                        });
                    }
                    list = list.push(line);
                    if matches!(
                        status,
                        InstanceStatus::Crashed | InstanceStatus::NeedsConfig | InstanceStatus::Unhealthy
                    ) {
                        if let Some(message) = &instance.runtime.message {
                            list = list.push(text(format!("     {message}")));
                        }
                        if let Some(issue) = &instance.runtime.issue {
                            list = list.push(text(format!("     {}", issue.detail)));
                        }
                    }
                }
            }
            if self.mirror.live.projects.is_empty() {
                list = list.push(text("No projects yet. Add a project or run lantern scan."));
            }
            body = body.push(scrollable(list).height(Length::Fill));
        }
        for (index, toast) in self.toasts.iter().enumerate() {
            body = body.push(
                row![
                    text(toast).width(Length::Fill),
                    button("Dismiss").on_press(Message::Dismiss(index))
                ]
                .spacing(8),
            );
        }
        container(body).width(Length::Fill).height(Length::Fill).into()
    }
    fn theme(&self, _: window::Id) -> Option<Theme> {
        use iced::{color, theme::Palette};
        let palette = if self.mode == theme::Mode::Light {
            Palette {
                background: color!(0xFAFAF8),
                text: color!(0x1F2328),
                primary: color!(0xC77D0A),
                success: color!(0x1A7F37),
                warning: color!(0x9A6700),
                danger: color!(0xCF222E),
            }
        } else {
            Palette {
                background: color!(0x14161A),
                text: color!(0xE6E6E6),
                primary: color!(0xF5A623),
                success: color!(0x3FB950),
                warning: color!(0xD29922),
                danger: color!(0xF85149),
            }
        };
        Some(Theme::custom("Lantern", palette))
    }
    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::close_requests().map(Message::Close),
            iced::system::theme_changes().map(Message::Theme),
            Subscription::run_with(ClientKey(self.client.clone(), self.stream_generation), stream),
        ];
        if !self.pending.is_empty() {
            subscriptions.push(iced::time::every(Duration::from_millis(500)).map(Message::Tick));
        }
        Subscription::batch(subscriptions)
    }
}
fn settled(kind: CommandKind, status: InstanceStatus) -> bool {
    matches!(status, InstanceStatus::Crashed | InstanceStatus::NeedsConfig)
        || match kind {
            CommandKind::Start | CommandKind::Restart => {
                matches!(status, InstanceStatus::Running | InstanceStatus::Unhealthy)
            }
            CommandKind::Stop => status == InstanceStatus::Stopped,
        }
}
async fn daemon_action(show: bool) -> Result<String, String> {
    let args: &[&str] = if show {
        &["--user", "status", "lantern", "--no-pager"]
    } else {
        &["--user", "start", "lantern"]
    };
    let output = tokio::process::Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if show {
        Ok(format!("{stdout}{stderr}"))
    } else if output.status.success() {
        Ok(String::new())
    } else {
        Err(format!(
            "Could not start daemon: {}",
            stderr.lines().next().unwrap_or("systemctl failed")
        ))
    }
}
#[derive(Debug, Clone)]
struct ClientKey(Client, u64);
impl std::hash::Hash for ClientKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        "lantern-state-stream".hash(state);
        self.1.hash(state);
    }
}
fn stream(key: &ClientKey) -> impl futures_util::Stream<Item = Message> + use<> {
    key.0
        .events(EventsOptions::default())
        .map(|event| Message::Stream(Box::new(event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{hello, instance, project};
    use lantern_core::api::WireEvent;
    fn app() -> App {
        let args = GuiArgs {
            tray: true,
            ..GuiArgs::default()
        };
        let client = Client::new(lantern_client::ClientConfig {
            base_url: Some("http://127.0.0.1:39300".into()),
        })
        .unwrap();
        App::boot(
            &args,
            Fonts {
                sans: "sans-serif",
                mono: "monospace",
            },
            client,
        )
        .0
    }
    fn complete(app: &mut App) {
        for event in [
            hello("test"),
            WireEvent::ProjectUpdated { project: project() },
            WireEvent::InstanceUpdated {
                instance: instance(InstanceStatus::Stopped),
            },
            WireEvent::SnapshotComplete,
        ] {
            drop(app.update(Message::Stream(Box::new(StreamEvent::Event(event)))));
        }
    }
    #[test]
    fn dashboard_smoke_shows_group_and_pending() {
        let mut app = app();
        complete(&mut app);
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("Example   /tmp/example").is_ok());
        assert!(ui.find("Start").is_ok());
        ui.click("Start").unwrap();
        let messages: Vec<_> = ui.into_messages().collect();
        assert_eq!(messages.len(), 1);
        drop(app.update(messages.into_iter().next().unwrap()));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("◌ Working…").is_ok());
        assert!(ui.find("Start").is_err());
    }
    #[test]
    fn down_screen_offers_daemon_recovery() {
        let mut app = app();
        drop(app.update(Message::Stream(Box::new(StreamEvent::Lost {
            error: "offline".into(),
            daemon_unreachable: true,
            attempt: 0,
            retry_in: Duration::from_secs(1),
        }))));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("Start daemon").is_ok());
        assert!(ui.find("Retry now").is_ok());
    }
}
