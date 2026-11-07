use crate::fonts::Fonts;
use crate::model::{Connection, Effect, Mirror};
use futures_util::StreamExt;
use iced::{
    Element, Font, Subscription, Task, Theme, theme,
    widget::{container, text},
    window,
};
use lantern_cli::args::GuiArgs;
use lantern_client::{Client, EventsOptions, StreamEvent};

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

#[derive(Debug)]
struct App {
    window: Option<window::Id>,
    mode: theme::Mode,
    fonts: Fonts,
    client: Client,
    mirror: Mirror,
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    Close(window::Id),
    Theme(theme::Mode),
    Stream(Box<StreamEvent>),
}

impl App {
    fn boot(args: &GuiArgs, fonts: Fonts, client: Client) -> (Self, Task<Message>) {
        let mut app = Self {
            window: None,
            mode: theme::Mode::Dark,
            fonts,
            client,
            mirror: Mirror::default(),
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
                        Effect::Transition { id, status } => {
                            let _ = (id, status);
                        }
                        Effect::Progress {
                            op,
                            instance,
                            step,
                            done,
                            error,
                        } => {
                            let _ = (op, instance, step, done, error);
                        }
                        Effect::Removed(id) => {
                            let _ = id;
                        }
                        Effect::Restarted | Effect::Resynced => {}
                    }
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
        container(
            text(match &self.mirror.connection {
                Connection::Connecting => "Connecting to Lantern…".to_string(),
                Connection::Live => format!("{} projects connected", self.mirror.live.projects.len()),
                Connection::Down { error, .. } => format!("The Lantern daemon is not running: {error}"),
            })
            .font(Font::with_name(self.fonts.mono))
            .size(24),
        )
        .padding(24)
        .into()
    }
    fn theme(&self, _: window::Id) -> Option<Theme> {
        Some(match self.mode {
            theme::Mode::Light => Theme::Light,
            theme::Mode::Dark | theme::Mode::None => Theme::Dark,
        })
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::close_requests().map(Message::Close),
            iced::system::theme_changes().map(Message::Theme),
            Subscription::run_with(ClientKey(self.client.clone()), stream),
        ])
    }
}

#[derive(Debug, Clone)]
struct ClientKey(Client);
impl std::hash::Hash for ClientKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        "lantern-state-stream".hash(state);
    }
}
fn stream(key: &ClientKey) -> impl futures_util::Stream<Item = Message> + use<> {
    key.0
        .events(EventsOptions::default())
        .map(|event| Message::Stream(Box::new(event)))
}
