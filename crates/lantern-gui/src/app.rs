use crate::fonts::Fonts;
use iced::{
    Element, Font, Subscription, Task, Theme, theme,
    widget::{container, text},
    window,
};
use lantern_cli::args::GuiArgs;

pub(crate) fn run(args: GuiArgs, fonts: Fonts) -> iced::Result {
    iced::daemon(move || App::boot(&args, fonts), App::update, App::view)
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
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    Close(window::Id),
    Theme(theme::Mode),
}

impl App {
    fn boot(args: &GuiArgs, fonts: Fonts) -> (Self, Task<Message>) {
        let mut app = Self {
            window: None,
            mode: theme::Mode::Dark,
            fonts,
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
            Message::Theme(mode) => {
                self.mode = mode;
                Task::none()
            }
        }
    }
    fn view(&self, _: window::Id) -> Element<'_, Message> {
        container(
            text("Connecting to Lantern…")
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
        ])
    }
}
