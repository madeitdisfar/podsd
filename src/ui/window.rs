use iced::widget::button::Style;
use iced::widget::{button, column, container, pane_grid, text, Space};
use iced::{daemon, window, Background, Element, Length, Subscription, Task, Theme};
use std::sync::Arc;
use log::debug;
use tokio::sync::{mpsc::UnboundedReceiver, Mutex};

pub fn start_ui(ui_rx: UnboundedReceiver<()>, start_minimized: bool) -> iced::Result {
    daemon(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .run_with(move || App::new(ui_rx, start_minimized))
}

pub struct App {
    window: Option<window::Id>,
    panes: pane_grid::State<Pane>,
    selected_tab: Tab,
    ui_rx: Arc<Mutex<UnboundedReceiver<()>>>,
}

#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    Resized(pane_grid::ResizeEvent),
    SelectTab(Tab),
    OpenMainWindow,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tab {
    Device1,
    Device2,
    Device3,
    Device4,
    Settings,
}

#[derive(Clone, Copy)]
pub enum Pane {
    Sidebar,
    Content,
}

impl App {
    pub fn new(ui_rx: UnboundedReceiver<()>, start_minimized: bool) -> (Self, Task<Message>) {
        let (mut panes, first_pane) = pane_grid::State::new(Pane::Sidebar);
        let split = panes.split(pane_grid::Axis::Vertical, first_pane, Pane::Content);
        panes.resize(split.unwrap().1, 0.2);

        let ui_rx = Arc::new(Mutex::new(ui_rx));
        let wait_task = Task::perform(
            wait_for_message(Arc::clone(&ui_rx)),
            |_| Message::OpenMainWindow,
        );

        let (window, open_task) = if start_minimized {
            (None, Task::none())
        } else {
            let (id, open) = window::open(window::Settings::default());
            (Some(id), open.map(Message::WindowOpened))
        };

        (
            Self {
                window,
                panes,
                selected_tab: Tab::Device1,
                ui_rx,
            },
            Task::batch(vec![open_task, wait_task]),
        )
    }

    fn title(&self, _id: window::Id) -> String {
        "LibrePods".to_string()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.window = Some(id);
                Task::none()
            }
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                let wait_task = Task::perform(
                    wait_for_message(Arc::clone(&self.ui_rx)),
                    |_| Message::OpenMainWindow,
                );
                wait_task
            }
            Message::Resized(event) => {
                self.panes.resize(event.split, event.ratio);
                Task::none()
            }
            Message::SelectTab(tab) => {
                self.selected_tab = tab;
                Task::none()
            }
            Message::OpenMainWindow => {
                if let Some(window_id) = self.window {
                    Task::batch(vec![
                        window::minimize(window_id, false),
                        window::gain_focus(window_id),
                    ])
                } else {
                    let (new_window_task, open_task) = window::open(window::Settings::default());
                    self.window = Some(new_window_task);
                    open_task.map(Message::WindowOpened)
                }
            }
        }
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let pane_grid = pane_grid::PaneGrid::new(&self.panes, |_pane_id, pane, _is_maximized| {
            match pane {
                Pane::Sidebar => {
                    let create_tab_button = |tab: Tab, label: &str| -> Element<'_, Message> {
                        let label = label.to_string();
                        let is_selected = self.selected_tab == tab;
                        let content = container(text(label).size(18)).padding(10);
                        let style = move |theme: &Theme, _status| {
                            if is_selected {
                                Style::default()
                                    .with_background(Background::Color(theme.palette().primary))
                            } else {
                                let mut style = Style::default();
                                style.text_color = theme.palette().text;
                                style
                            }
                        };
                        button(content)
                            .style(style)
                            .on_press(Message::SelectTab(tab))
                            .width(Length::Fill)
                            .into()
                    };

                    let devices = column![
                        create_tab_button(Tab::Device1, "Device 1"),
                        create_tab_button(Tab::Device2, "Device 2"),
                        create_tab_button(Tab::Device3, "Device 3"),
                        create_tab_button(Tab::Device4, "Device 4")
                    ]
                    .spacing(5);

                    let settings = create_tab_button(Tab::Settings, "Settings");

                    let content = column![
                        devices,
                        Space::with_height(Length::Fill),
                        settings
                    ]
                    .spacing(5);

                    pane_grid::Content::new(content)
                }
                Pane::Content => {
                    let content_text = match self.selected_tab {
                        Tab::Device1 => "Content for Device 1",
                        Tab::Device2 => "Content for Device 2",
                        Tab::Device3 => "Content for Device 3",
                        Tab::Device4 => "Content for Device 4",
                        Tab::Settings => "Settings content",
                    };
                    let content = container(text(content_text).size(40))
                        .center_x(Length::Fill)
                        .center_y(Length::Fill);

                    pane_grid::Content::new(content)
                }
            }
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .on_resize(20, Message::Resized);

        container(pane_grid).into()
    }

    fn theme(&self, _id: window::Id) -> Theme {
        Theme::Moonfly
    }

    fn subscription(&self) -> Subscription<Message> {
        window::close_events().map(Message::WindowClosed)
    }
}

async fn wait_for_message(rx: Arc<Mutex<UnboundedReceiver<()>>>) {
    debug!("Waiting for message to open main window...");
    let mut guard = rx.lock().await;
    let _ = guard.recv().await;
}