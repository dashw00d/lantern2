use crate::fonts::Fonts;
use crate::logs::{LogBuffer, ROW_HEIGHT};
use crate::model::{Connection, Effect, Mirror};
use crate::platform::{UiEvent, UiReceiver, tray::TraySnapshot};
use crate::prefs::{GuiPrefs, PrefsStore, ThemePreference};
use futures_util::StreamExt;
use iced::{
    Element, Font, Length, Subscription, Task, Theme, keyboard, theme,
    widget::{button, column, container, row, scrollable, space, text, text_input},
    window,
};
use lantern_cli::args::GuiArgs;
use lantern_client::{Client, EventsOptions, StreamEvent, Wait};
use lantern_core::{
    api::{LogPage, LogParams, OperationId, OperationResponse, OperationStatus, WireEvent},
    ids::InstanceId,
    model::{InstanceStatus, LogLine},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(crate) fn run(
    args: GuiArgs,
    fonts: Fonts,
    client: Client,
    ui_rx: UiReceiver,
    tray_tx: tokio::sync::watch::Sender<TraySnapshot>,
) -> iced::Result {
    let ui_key = UiKey(Arc::new(Mutex::new(Some(ui_rx))));
    let prefs_store = PrefsStore::from_env();
    let (prefs, prefs_error) = match prefs_store.as_ref().map(PrefsStore::load).transpose() {
        Ok(prefs) => (prefs.unwrap_or_default(), None),
        Err(error) => (
            GuiPrefs::default(),
            Some(format!("Could not load GUI preferences: {error}")),
        ),
    };
    iced::daemon(
        move || {
            App::boot(
                &args,
                fonts,
                client.clone(),
                ui_key.clone(),
                tray_tx.clone(),
                (prefs_store.clone(), prefs.clone(), prefs_error.clone()),
            )
        },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Running,
    Attention,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectTab {
    Overview,
    Logs,
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
    request_id: u64,
    early: BTreeMap<OperationId, (String, bool, Option<String>)>,
}

#[derive(Debug)]
struct App {
    window: Option<window::Id>,
    mode: theme::Mode,
    fonts: Fonts,
    client: Client,
    mirror: Mirror,
    pending: BTreeMap<InstanceId, Pending>,
    next_request_id: u64,
    toasts: Vec<String>,
    filter: String,
    why: Option<String>,
    stream_generation: u64,
    log_target: Option<InstanceId>,
    project_tab: ProjectTab,
    status_filter: StatusFilter,
    logs: LogBuffer,
    log_loading: bool,
    log_epoch: u64,
    log_live: VecDeque<LogLine>,
    ui_key: UiKey,
    tray_tx: tokio::sync::watch::Sender<TraySnapshot>,
    tray_available: Option<bool>,
    prefs: GuiPrefs,
    prefs_store: Option<PrefsStore>,
    prefs_saving: bool,
    quick_open: bool,
    quick_query: String,
    quick_index: usize,
}
#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    Close(window::Id),
    Theme(theme::Mode),
    Stream(Box<StreamEvent>),
    Command(Command),
    CommandDone(InstanceId, u64, Box<Result<OperationResponse, String>>),
    Tick(Instant),
    Filter(String),
    Dismiss(usize),
    Retry,
    StartDaemon,
    ShowWhy,
    DaemonDone(Result<String, String>, bool),
    CopyCommand,
    OpenLogs(InstanceId),
    OpenProject(InstanceId),
    ProjectTab(ProjectTab),
    StatusFilter(StatusFilter),
    Dashboard,
    LogBacklog(InstanceId, u64, Result<LogPage, String>),
    LogScrolled(scrollable::Viewport),
    LogFollow,
    Ui(UiEvent),
    CycleTheme,
    PrefsSaved(Result<(), String>),
    Key(keyboard::Key, keyboard::Modifiers),
    OpenQuick,
    QuickQuery(String),
    QuickActivate(bool),
    QuickChoose(InstanceId, bool),
    OpenUrlDone(Result<(), String>),
}
impl App {
    fn boot(
        args: &GuiArgs,
        fonts: Fonts,
        client: Client,
        ui_key: UiKey,
        tray_tx: tokio::sync::watch::Sender<TraySnapshot>,
        initial_prefs: (Option<PrefsStore>, GuiPrefs, Option<String>),
    ) -> (Self, Task<Message>) {
        let (prefs_store, prefs, prefs_error) = initial_prefs;
        let mut app = Self {
            window: None,
            mode: theme::Mode::Dark,
            fonts,
            client,
            mirror: Mirror::default(),
            pending: BTreeMap::new(),
            next_request_id: 0,
            toasts: prefs_error.into_iter().collect(),
            filter: String::new(),
            why: None,
            stream_generation: 0,
            log_target: None,
            project_tab: ProjectTab::Logs,
            status_filter: StatusFilter::All,
            logs: LogBuffer::default(),
            log_loading: false,
            log_epoch: 0,
            log_live: VecDeque::new(),
            ui_key,
            tray_tx,
            tray_available: None,
            prefs,
            prefs_store,
            prefs_saving: false,
            quick_open: false,
            quick_query: String::new(),
            quick_index: 0,
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
    fn show_window(&mut self) -> Task<Message> {
        if let Some(id) = self.window {
            Task::batch([
                window::gain_focus(id),
                window::request_user_attention(id, Some(window::UserAttention::Informational)),
            ])
        } else {
            self.open()
        }
    }
    fn quick_results(&self) -> Vec<&lantern_core::api::InstanceView> {
        let query = self
            .quick_query
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        let mut matches: Vec<_> = self
            .mirror
            .live
            .instances
            .values()
            .filter(|i| i.visible)
            .filter_map(|instance| {
                let haystack = format!(
                    "{} {}",
                    instance.id,
                    instance.checkout.branch.as_deref().unwrap_or("")
                )
                .to_lowercase();
                fuzzy_score(&haystack, &query).map(|score| (score, instance))
            })
            .collect();
        matches.sort_by(|(a, left), (b, right)| a.cmp(b).then_with(|| left.id.cmp(&right.id)));
        matches.into_iter().map(|(_, instance)| instance).collect()
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
                if self.tray_available == Some(true) {
                    window::close(id)
                } else {
                    iced::exit()
                }
            }
            Message::Ui(event) => match event {
                UiEvent::ShowWindow(_token) => self.show_window(),
                UiEvent::ShowTarget(target) => {
                    match InstanceId::parse(&target) {
                        Ok(id) => {
                            self.log_target = Some(id);
                            self.project_tab = ProjectTab::Overview;
                            self.logs = LogBuffer::default();
                            self.stream_generation = self.stream_generation.wrapping_add(1);
                        }
                        Err(error) => self.toasts.push(format!("Invalid instance: {error}")),
                    }
                    self.show_window()
                }
                UiEvent::Stop(id) => self.update(Message::Command(Command {
                    id,
                    kind: CommandKind::Stop,
                })),
                UiEvent::StartDaemon => self.update(Message::StartDaemon),
                UiEvent::TrayAvailable(available) => {
                    self.tray_available = Some(available);
                    if !available {
                        self.toasts
                            .push("No system tray found; closing the window will quit Lantern.".into());
                        if self.window.is_none() {
                            return self.open();
                        }
                    }
                    Task::none()
                }
                UiEvent::Quit => iced::exit(),
            },
            Message::Stream(event) => {
                if matches!(&*event, StreamEvent::Connecting { .. } | StreamEvent::Lost { .. })
                    && self.log_target.is_some()
                {
                    // A backlog reply from the previous connection must never overwrite this view.
                    self.log_epoch = self.log_epoch.wrapping_add(1);
                    self.log_loading = false;
                    self.log_live.clear();
                }
                if matches!(&*event, StreamEvent::Connected)
                    && let Some(id) = self.log_target.clone()
                {
                    self.mirror.stream(StreamEvent::Connected);
                    let _ = self.tray_tx.send(TraySnapshot::from_mirror(&self.mirror));
                    self.log_epoch = self.log_epoch.wrapping_add(1);
                    let epoch = self.log_epoch;
                    self.log_loading = true;
                    self.log_live.clear();
                    let client = self.client.clone();
                    let requested = id.clone();
                    return Task::perform(
                        async move {
                            client
                                .logs(
                                    &requested,
                                    LogParams {
                                        tail: Some(5000),
                                        since: None,
                                        stream: None,
                                        grep: None,
                                        format: None,
                                    },
                                )
                                .await
                                .map_err(|e| e.to_string())
                        },
                        move |result| Message::LogBacklog(id.clone(), epoch, result),
                    );
                }
                if let StreamEvent::Event(WireEvent::Log { line }) = &*event {
                    if self.log_target.as_ref() == Some(&line.instance) {
                        if self.log_loading {
                            if self.log_live.len() == 20_000 {
                                self.log_live.pop_front();
                            }
                            self.log_live.push_back(line.clone());
                        } else {
                            self.logs.push(line.clone());
                            if self.logs.follow {
                                return iced::widget::operation::snap_to_end("lantern-logs");
                            }
                        }
                    }
                    return Task::none();
                }
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
                            if let Some(id) = &self.log_target
                                && !self.mirror.live.instances.contains_key(id)
                            {
                                self.toasts.push(format!("{id} is no longer available"));
                                self.log_target = None;
                                self.log_epoch = self.log_epoch.wrapping_add(1);
                                self.stream_generation = self.stream_generation.wrapping_add(1);
                            }
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
                            if let Some(pending) = self.pending.get_mut(&instance) {
                                if pending.operation.is_none() {
                                    if pending.early.len() == 8 && !pending.early.contains_key(&op) {
                                        pending.early.pop_first();
                                    }
                                    pending.early.insert(op, (step, done, error));
                                } else if pending.operation.as_ref() == Some(&op) {
                                    if done {
                                        self.pending.remove(&instance);
                                        if let Some(error) = error {
                                            self.toasts.push(error);
                                        }
                                    } else {
                                        pending.step = step;
                                    }
                                }
                            }
                        }
                        Effect::Removed(id) => {
                            self.pending.remove(&id);
                            if self.log_target.as_ref() == Some(&id) {
                                self.log_target = None;
                                self.stream_generation = self.stream_generation.wrapping_add(1);
                                self.toasts.push(format!("{id} was removed"));
                            }
                        }
                    }
                }
                let _ = self.tray_tx.send(TraySnapshot::from_mirror(&self.mirror));
                Task::none()
            }
            Message::Command(cmd) => {
                if !matches!(self.mirror.connection, Connection::Live) || self.pending.contains_key(&cmd.id) {
                    return Task::none();
                }
                let id = cmd.id.clone();
                self.next_request_id = self.next_request_id.wrapping_add(1);
                let request_id = self.next_request_id;
                self.pending.insert(
                    id.clone(),
                    Pending {
                        kind: cmd.kind,
                        since: Instant::now(),
                        operation: None,
                        step: "Working…".into(),
                        request_id,
                        early: BTreeMap::new(),
                    },
                );
                let client = self.client.clone();
                Task::perform(async move { cmd.execute(&client).await }, move |result| {
                    Message::CommandDone(id.clone(), request_id, Box::new(result))
                })
            }
            Message::CommandDone(id, request_id, result) => {
                let matches = self
                    .pending
                    .get(&id)
                    .is_some_and(|pending| pending.request_id == request_id);
                match *result {
                    Err(error) => {
                        if matches {
                            self.pending.remove(&id);
                        }
                        self.toasts.push(error);
                    }
                    Ok(response) => {
                        if !matches {
                            return Task::none();
                        }
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
                            let operation = response.operation.id;
                            if let Some((step, done, error)) = pending.early.remove(&operation) {
                                if done {
                                    self.pending.remove(&id);
                                    if let Some(error) = error {
                                        self.toasts.push(error);
                                    }
                                } else {
                                    pending.step = step;
                                    pending.operation = Some(operation);
                                    pending.early.clear();
                                }
                            } else {
                                pending.operation = Some(operation);
                                pending.early.clear();
                            }
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
            Message::OpenLogs(id) => {
                self.log_target = Some(id);
                self.project_tab = ProjectTab::Logs;
                self.logs = LogBuffer::default();
                self.log_live.clear();
                self.log_epoch = self.log_epoch.wrapping_add(1);
                self.stream_generation = self.stream_generation.wrapping_add(1);
                Task::none()
            }
            Message::OpenProject(id) => {
                let task = self.update(Message::OpenLogs(id));
                self.project_tab = ProjectTab::Overview;
                task
            }
            Message::ProjectTab(tab) => {
                self.project_tab = tab;
                Task::none()
            }
            Message::StatusFilter(filter) => {
                self.status_filter = filter;
                Task::none()
            }
            Message::Dashboard => {
                self.log_target = None;
                self.log_epoch = self.log_epoch.wrapping_add(1);
                self.stream_generation = self.stream_generation.wrapping_add(1);
                Task::none()
            }
            Message::LogBacklog(id, epoch, result) => {
                if self.log_target.as_ref() != Some(&id) || self.log_epoch != epoch {
                    return Task::none();
                }
                self.log_loading = false;
                let incoming = std::mem::take(&mut self.log_live);
                match result {
                    Ok(page) => self
                        .logs
                        .replace_with_backlog(page.lines, incoming.into_iter().collect()),
                    Err(error) => {
                        self.toasts.push(format!("Could not load logs: {error}"));
                        for line in incoming {
                            self.logs.push(line);
                        }
                    }
                }
                if self.logs.follow {
                    iced::widget::operation::snap_to_end("lantern-logs")
                } else {
                    Task::none()
                }
            }
            Message::LogScrolled(viewport) => {
                self.logs.scroll_y = viewport.absolute_offset().y;
                self.logs.viewport_h = viewport.bounds().height;
                self.logs.follow = viewport.absolute_offset_reversed().y < 2.0 * ROW_HEIGHT;
                Task::none()
            }
            Message::LogFollow => {
                self.logs.follow = true;
                iced::widget::operation::snap_to_end("lantern-logs")
            }
            Message::CycleTheme => {
                if self.prefs_saving {
                    return Task::none();
                }
                self.prefs.theme = self.prefs.theme.next();
                if let Some(store) = self.prefs_store.clone() {
                    self.prefs_saving = true;
                    let prefs = self.prefs.clone();
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || store.save(&prefs))
                                .await
                                .map_err(|e| e.to_string())?
                                .map_err(|e| e.to_string())
                        },
                        Message::PrefsSaved,
                    )
                } else {
                    Task::none()
                }
            }
            Message::PrefsSaved(result) => {
                self.prefs_saving = false;
                if let Err(error) = result {
                    self.toasts
                        .push(format!("Could not save GUI preferences: {error}"));
                }
                Task::none()
            }
            Message::Key(key, modifiers) => {
                use keyboard::{Key, key::Named};
                match key {
                    Key::Character(value) if modifiers.control() && value.eq_ignore_ascii_case("k") => {
                        self.update(Message::OpenQuick)
                    }
                    Key::Named(Named::Escape) if self.quick_open => {
                        self.quick_open = false;
                        Task::none()
                    }
                    Key::Named(Named::ArrowDown) if self.quick_open => {
                        self.quick_index =
                            (self.quick_index + 1).min(self.quick_results().len().min(10).saturating_sub(1));
                        Task::none()
                    }
                    Key::Named(Named::ArrowUp) if self.quick_open => {
                        self.quick_index = self.quick_index.saturating_sub(1);
                        Task::none()
                    }
                    Key::Named(Named::Enter) if self.quick_open => {
                        self.update(Message::QuickActivate(modifiers.control()))
                    }
                    _ => Task::none(),
                }
            }
            Message::OpenQuick => {
                self.quick_open = true;
                self.quick_query.clear();
                self.quick_index = 0;
                iced::widget::operation::focus("lantern-quick")
            }
            Message::QuickQuery(query) => {
                self.quick_query = query;
                self.quick_index = 0;
                Task::none()
            }
            Message::QuickActivate(logs) => {
                if let Some(id) = self
                    .quick_results()
                    .get(self.quick_index)
                    .map(|instance| instance.id.clone())
                {
                    self.update(Message::QuickChoose(id, logs))
                } else {
                    Task::none()
                }
            }
            Message::QuickChoose(id, logs) => {
                self.quick_open = false;
                if logs {
                    return self.update(Message::OpenLogs(id));
                }
                let Some(instance) = self.mirror.live.instances.get(&id) else {
                    return Task::none();
                };
                if !matches!(self.mirror.connection, Connection::Live) {
                    self.toasts
                        .push("Reconnect to the daemon before starting or opening an instance.".into());
                    return Task::none();
                }
                if matches!(
                    instance.runtime.status,
                    InstanceStatus::Running | InstanceStatus::Unhealthy
                ) {
                    let url = instance.url.clone();
                    if !url.starts_with("https://") && !url.starts_with("http://") {
                        self.toasts
                            .push("Only HTTP(S) instance URLs can be opened.".into());
                        return Task::none();
                    }
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || open::that(url))
                                .await
                                .map_err(|e| e.to_string())?
                                .map_err(|e| e.to_string())
                        },
                        Message::OpenUrlDone,
                    )
                } else {
                    self.update(Message::Command(Command {
                        id,
                        kind: CommandKind::Start,
                    }))
                }
            }
            Message::OpenUrlDone(result) => {
                if let Err(error) = result {
                    self.toasts.push(format!("Could not open URL: {error}"));
                }
                Task::none()
            }
            Message::Theme(mode) => {
                self.mode = mode;
                Task::none()
            }
        }
    }
    fn view(&self, _: window::Id) -> Element<'_, Message> {
        let theme_button = button(text(format!("Theme: {}", self.prefs.theme.label())));
        let theme_button = if self.prefs_saving {
            theme_button
        } else {
            theme_button.on_press(Message::CycleTheme)
        };
        let mut body = column![
            row![
                text("Lantern").size(28).width(Length::Fill),
                button("Quick switch  Ctrl+K").on_press(Message::OpenQuick),
                theme_button
            ]
            .spacing(12)
        ]
        .spacing(12)
        .padding(20);
        if self.quick_open {
            body = body.push(text(
                "Quick switch  ·  Enter: open or start  ·  Ctrl+Enter: logs  ·  Esc: close",
            ));
            body = body.push(
                text_input("Project or branch", &self.quick_query)
                    .id("lantern-quick")
                    .on_input(Message::QuickQuery),
            );
            for (index, instance) in self.quick_results().iter().take(10).enumerate() {
                let marker = if index == self.quick_index { "›" } else { " " };
                let label = format!(
                    "{marker}  {}   {:?}   {}",
                    instance.id,
                    instance.runtime.status,
                    instance.checkout.branch.as_deref().unwrap_or("detached")
                );
                body =
                    body.push(button(text(label)).on_press(Message::QuickChoose(instance.id.clone(), false)));
            }
        }
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
        if let Some(target) = &self.log_target {
            body = body.push(
                row![
                    button("‹ Dashboard").on_press(Message::Dashboard),
                    text(format!("{target}")).size(22),
                    button("Overview").on_press(Message::ProjectTab(ProjectTab::Overview)),
                    button("Logs").on_press(Message::ProjectTab(ProjectTab::Logs))
                ]
                .spacing(12),
            );
            if self.project_tab == ProjectTab::Overview {
                if let Some(instance) = self.mirror.live.instances.get(target) {
                    body = body.push(text(format!(
                        "Status: {:?}   {}",
                        instance.runtime.status, instance.url
                    )));
                    body = body.push(text(format!("Checkout: {}", instance.checkout.path.display())));
                    body = body.push(text(format!(
                        "Branch: {}",
                        instance.checkout.branch.as_deref().unwrap_or("detached")
                    )));
                    if let Some(message) = &instance.runtime.message {
                        body = body.push(text(format!("Why: {message}")));
                    }
                    if let Some(issue) = &instance.runtime.issue {
                        body = body.push(text(format!(
                            "{} {}",
                            issue.detail,
                            issue.hint.as_deref().unwrap_or_default()
                        )));
                    }
                } else {
                    body = body.push(text("Waiting for instance state…"));
                }
            } else {
                body = body.push(button("Jump to latest").on_press(Message::LogFollow));
                body = body.push(text(format!(
                    "{} lines buffered · {}",
                    self.logs.len(),
                    if self.logs.follow { "Following" } else { "Paused" }
                )));
                let range = self.logs.range();
                let mut rows = column![space::vertical().height(range.start as f32 * ROW_HEIGHT)].spacing(0);
                for index in range.clone() {
                    if let Some(line) = self.logs.get(index) {
                        let value = format!("{}  {}", line.ts, line.line);
                        rows = rows.push(
                            container(
                                text(value)
                                    .font(Font::with_name(self.fonts.mono))
                                    .size(13)
                                    .wrapping(text::Wrapping::None),
                            )
                            .height(ROW_HEIGHT),
                        );
                    }
                }
                rows = rows.push(space::vertical().height((self.logs.len() - range.end) as f32 * ROW_HEIGHT));
                body = body.push(
                    scrollable(rows)
                        .id("lantern-logs")
                        .on_scroll(Message::LogScrolled)
                        .height(Length::Fill),
                );
            }
        } else if self.mirror.ever_live {
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
            let unmanaged = self
                .mirror
                .live
                .listeners
                .iter()
                .filter(|listener| !listener.managed)
                .count();
            body = body
                .push(
                    row![
                        button("All").on_press(Message::StatusFilter(StatusFilter::All)),
                        button(text(format!("{running} running")))
                            .on_press(Message::StatusFilter(StatusFilter::Running)),
                        button(text(format!("{attention} needs attention")))
                            .on_press(Message::StatusFilter(StatusFilter::Attention)),
                        text(format!("{unmanaged} unmanaged"))
                    ]
                    .spacing(8),
                )
                .push(text_input("Filter projects or instances", &self.filter).on_input(Message::Filter));
            let filter = self.filter.to_lowercase();
            let mut list = column![].spacing(8);
            for project in self.mirror.live.projects.values() {
                let mut children: Vec<_> = instances
                    .iter()
                    .copied()
                    .filter(|i| {
                        i.project == project.project.id
                            && match self.status_filter {
                                StatusFilter::All => true,
                                StatusFilter::Running => matches!(
                                    i.runtime.status,
                                    InstanceStatus::Running | InstanceStatus::Starting
                                ),
                                StatusFilter::Attention => matches!(
                                    i.runtime.status,
                                    InstanceStatus::Crashed
                                        | InstanceStatus::Unhealthy
                                        | InstanceStatus::NeedsConfig
                                ),
                            }
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
                    let color = match status {
                        InstanceStatus::Running => iced::color!(0x3FB950),
                        InstanceStatus::Crashed | InstanceStatus::NeedsConfig => iced::color!(0xF85149),
                        InstanceStatus::Unhealthy
                        | InstanceStatus::Starting
                        | InstanceStatus::Preparing
                        | InstanceStatus::Stopping => iced::color!(0xD29922),
                        _ => iced::color!(0x999999),
                    };
                    let mut line = row![
                        text(glyph).color(color),
                        text(format!("{name}   {branch}   {port}   {}", instance.url)).width(Length::Fill)
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
                    line = line.push(button("Overview").on_press(Message::OpenProject(instance.id.clone())));
                    line = line.push(button("Logs").on_press(Message::OpenLogs(instance.id.clone())));
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
            if unmanaged > 0 && self.status_filter == StatusFilter::All {
                list = list.push(text(format!("Other listeners ({unmanaged})")).size(20));
                for listener in self
                    .mirror
                    .live
                    .listeners
                    .iter()
                    .filter(|listener| !listener.managed)
                {
                    list = list.push(text(format!(
                        "◇ :{}   {}   {}",
                        listener.port,
                        listener.label,
                        listener
                            .cwd
                            .as_ref()
                            .map_or_else(String::new, |p| p.display().to_string())
                    )));
                }
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
        let light = match self.prefs.theme {
            ThemePreference::Light => true,
            ThemePreference::Dark => false,
            ThemePreference::System => self.mode == theme::Mode::Light,
        };
        let palette = if light {
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
            Subscription::run_with(
                ClientKey(
                    self.client.clone(),
                    self.stream_generation,
                    self.log_target.clone(),
                ),
                stream,
            ),
            Subscription::run_with(self.ui_key.clone(), ui_stream),
            iced::event::listen_with(|event, _, _| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    Some(Message::Key(key, modifiers))
                }
                _ => None,
            }),
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
fn fuzzy_score(haystack: &str, query: &str) -> Option<usize> {
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut last = 0;
    let mut score = 0;
    for (index, c) in haystack.chars().enumerate() {
        if Some(c) == next {
            score += index.saturating_sub(last);
            last = index + 1;
            next = wanted.next();
        }
    }
    if next.is_none() { Some(score) } else { None }
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
struct ClientKey(Client, u64, Option<InstanceId>);
impl std::hash::Hash for ClientKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        "lantern-state-stream".hash(state);
        self.1.hash(state);
        self.2.hash(state);
    }
}
fn stream(key: &ClientKey) -> impl futures_util::Stream<Item = Message> + use<> {
    key.0
        .events(EventsOptions {
            logs: key.2.clone().into_iter().collect(),
            tail: 1000,
            ..EventsOptions::default()
        })
        .map(|event| Message::Stream(Box::new(event)))
}

#[derive(Debug, Clone)]
struct UiKey(Arc<Mutex<Option<UiReceiver>>>);
impl std::hash::Hash for UiKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        "lantern-platform-events".hash(state);
    }
}
fn ui_stream(key: &UiKey) -> impl futures_util::Stream<Item = Message> + use<> {
    let receiver = key.0.lock().expect("UI receiver lock").take();
    futures_util::stream::unfold(receiver, |receiver| async move {
        let mut receiver = receiver?;
        receiver
            .recv()
            .await
            .map(|event| (Message::Ui(event), Some(receiver)))
    })
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
        let (_, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (tray, _) = tokio::sync::watch::channel(TraySnapshot::default());
        App::boot(
            &args,
            Fonts {
                sans: "sans-serif",
                mono: "monospace",
            },
            client,
            UiKey(Arc::new(Mutex::new(Some(receiver)))),
            tray,
            (None, GuiPrefs::default(), None),
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
    #[test]
    fn project_tabs_and_status_filters_are_headless() {
        let mut app = app();
        complete(&mut app);
        drop(app.update(Message::StatusFilter(StatusFilter::Running)));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("Start").is_err());
        drop(ui);
        drop(app.update(Message::StatusFilter(StatusFilter::All)));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        ui.click("Overview").unwrap();
        let messages: Vec<_> = ui.into_messages().collect();
        drop(app.update(messages.into_iter().next().unwrap()));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("Branch: main").is_ok());
        ui.click("Logs").unwrap();
        let messages: Vec<_> = ui.into_messages().collect();
        drop(app.update(messages.into_iter().next().unwrap()));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("Jump to latest").is_ok());
    }
    #[test]
    fn quick_switch_filters_and_starts_without_optimistic_state() {
        let mut app = app();
        complete(&mut app);
        drop(app.update(Message::Key(
            keyboard::Key::Character("k".into()),
            keyboard::Modifiers::CTRL,
        )));
        drop(app.update(Message::QuickQuery("exmpl".into())));
        let mut ui = iced_test::simulator(app.view(window::Id::unique()));
        assert!(ui.find("›  example   Stopped   main").is_ok());
        drop(ui);
        drop(app.update(Message::QuickActivate(false)));
        assert!(app.pending.contains_key(&InstanceId::parse("example").unwrap()));
        assert_eq!(
            app.mirror.live.instances[&InstanceId::parse("example").unwrap()]
                .runtime
                .status,
            InstanceStatus::Stopped
        );
    }
    #[test]
    fn pending_correlates_early_progress_and_stale_http_replies() {
        use lantern_core::api::{Operation, OperationKind};
        let mut app = app();
        complete(&mut app);
        let id = InstanceId::parse("example").unwrap();
        drop(app.update(Message::Command(Command {
            id: id.clone(),
            kind: CommandKind::Start,
        })));
        let request = app.pending[&id].request_id;
        let progress = |op: &str, done| {
            Message::Stream(Box::new(StreamEvent::Event(WireEvent::OperationProgress {
                op: OperationId(op.into()),
                instance: id.clone(),
                step: "starting".into(),
                done,
                error: None,
            })))
        };
        drop(app.update(progress("unrelated", true)));
        drop(app.update(progress("ours", true)));
        assert!(app.pending.contains_key(&id));
        let response = OperationResponse {
            operation: Operation {
                id: OperationId("ours".into()),
                kind: OperationKind::Start,
                instance: Some(id.clone()),
                status: OperationStatus::Running,
                noop: false,
                step: None,
                steps: vec![],
                created_at: "2026-11-06T00:00:00Z".parse().unwrap(),
                finished_at: None,
                error: None,
                result: None,
            },
            instance: None,
            created: None,
            forgotten: None,
        };
        drop(app.update(Message::CommandDone(id.clone(), request, Box::new(Ok(response)))));
        assert!(!app.pending.contains_key(&id));
        drop(app.update(Message::Command(Command {
            id: id.clone(),
            kind: CommandKind::Stop,
        })));
        drop(app.update(Message::CommandDone(
            id.clone(),
            request,
            Box::new(Err("late failure".into())),
        )));
        assert_eq!(app.pending[&id].kind, CommandKind::Stop);
    }
    #[test]
    fn old_backlog_reply_cannot_replace_logs_after_disconnect() {
        use lantern_core::model::Stream;
        let mut app = app();
        complete(&mut app);
        let id = InstanceId::parse("example").unwrap();
        drop(app.update(Message::OpenLogs(id.clone())));
        let line = LogLine {
            instance: id.clone(),
            stream: Stream::Stdout,
            line: "kept".into(),
            ts: "2026-11-06T00:00:00Z".parse().unwrap(),
        };
        app.logs.push(line);
        drop(app.update(Message::Stream(Box::new(StreamEvent::Connected))));
        let epoch = app.log_epoch;
        drop(app.update(Message::Stream(Box::new(StreamEvent::Lost {
            error: "offline".into(),
            daemon_unreachable: true,
            attempt: 0,
            retry_in: Duration::from_secs(1),
        }))));
        let page = LogPage {
            instance: id.clone(),
            lines: vec![],
            buffered: 0,
            truncated: false,
        };
        drop(app.update(Message::LogBacklog(id, epoch, Ok(page))));
        assert_eq!(app.logs.len(), 1);
        assert_eq!(app.logs.get(0).unwrap().line, "kept");
    }
    #[test]
    fn quick_switch_refuses_non_http_urls() {
        let mut app = app();
        complete(&mut app);
        let mut unsafe_url = instance(InstanceStatus::Running);
        unsafe_url.url = "file:///tmp/lantern-gui-test".into();
        let id = unsafe_url.id.clone();
        drop(app.update(Message::Stream(Box::new(StreamEvent::Event(
            WireEvent::InstanceUpdated { instance: unsafe_url },
        )))));
        drop(app.update(Message::QuickChoose(id, false)));
        assert!(app.toasts.iter().any(|toast| toast.contains("Only HTTP(S)")));
    }
}
