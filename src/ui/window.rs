
use std::collections::HashMap;
use iced::widget::button::Style;
use iced::widget::{button, column, container, pane_grid, text, Space, combo_box, row, text_input};
use iced::{daemon, window, Background, Border, Center, Color, Element, Length, Size, Subscription, Task, Theme};
use std::sync::Arc;
use iced::border::Radius;
use iced::overlay::menu;
use log::{debug, error};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;
use crate::bluetooth::aacp::{DeviceData, DeviceInformation, DeviceType};
use crate::ui::messages::UIMessage;
use crate::utils::{get_devices_path, get_app_settings_path, MyTheme};

pub fn start_ui(ui_rx: UnboundedReceiver<UIMessage>, start_minimized: bool) -> iced::Result {
    daemon(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .run_with(move || App::new(ui_rx, start_minimized))
}

pub struct App {
    window: Option<window::Id>,
    panes: pane_grid::State<Pane>,
    selected_tab: Tab,
    theme_state: combo_box::State<MyTheme>,
    selected_theme: MyTheme,
    ui_rx: Arc<Mutex<UnboundedReceiver<UIMessage>>>,
    bluetooth_state: BluetoothState
}

pub struct BluetoothState {
    connected_devices: Vec<String>
}

impl BluetoothState {
    pub fn new() -> Self {
        Self {
            connected_devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    Resized(pane_grid::ResizeEvent),
    SelectTab(Tab),
    ThemeSelected(MyTheme),
    CopyToClipboard(String),
    UIMessage(UIMessage),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Device(String),
    Settings,
}

#[derive(Clone, Copy)]
pub enum Pane {
    Sidebar,
    Content,
}


impl App {
    pub fn new(ui_rx: UnboundedReceiver<UIMessage>, start_minimized: bool) -> (Self, Task<Message>) {
        let (mut panes, first_pane) = pane_grid::State::new(Pane::Sidebar);
        let split = panes.split(pane_grid::Axis::Vertical, first_pane, Pane::Content);
        panes.resize(split.unwrap().1, 0.2);

        let ui_rx = Arc::new(Mutex::new(ui_rx));

        let wait_task = Task::perform(
            wait_for_message(Arc::clone(&ui_rx)),
            |msg| msg,
        );

        let (window, open_task) = if start_minimized {
            (None, Task::none())
        } else {
            let mut settings = window::Settings::default();
            settings.min_size = Some(Size::new(400.0, 300.0));
            settings.icon = window::icon::from_file("../../assets/icon.png").ok();
            let (id, open) = window::open(settings);
            (Some(id), open.map(Message::WindowOpened))
        };

        let app_settings_path = get_app_settings_path();
        let selected_theme = std::fs::read_to_string(&app_settings_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("theme").cloned())
            .and_then(|t| serde_json::from_value(t).ok())
            .unwrap_or(MyTheme::Dark);

        let bluetooth_state = BluetoothState::new();

        (
            Self {
                window,
                panes,
                selected_tab: Tab::Device("none".to_string()),
                theme_state: combo_box::State::new(vec![
                    MyTheme::Light,
                    MyTheme::Dark,
                    MyTheme::Dracula,
                    MyTheme::Nord,
                    MyTheme::SolarizedLight,
                    MyTheme::SolarizedDark,
                    MyTheme::GruvboxLight,
                    MyTheme::GruvboxDark,
                    MyTheme::CatppuccinLatte,
                    MyTheme::CatppuccinFrappe,
                    MyTheme::CatppuccinMacchiato,
                    MyTheme::CatppuccinMocha,
                    MyTheme::TokyoNight,
                    MyTheme::TokyoNightStorm,
                    MyTheme::TokyoNightLight,
                    MyTheme::KanagawaWave,
                    MyTheme::KanagawaDragon,
                    MyTheme::KanagawaLotus,
                    MyTheme::Moonfly,
                    MyTheme::Nightfly,
                    MyTheme::Oxocarbon,
                    MyTheme::Ferra,
                ]),
                selected_theme,
                ui_rx,
                bluetooth_state,
            },
            Task::batch(vec![open_task, wait_task])
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
                Task::none()
            }
            Message::Resized(event) => {
                self.panes.resize(event.split, event.ratio);
                Task::none()
            }
            Message::SelectTab(tab) => {
                self.selected_tab = tab;
                Task::none()
            }
            Message::ThemeSelected(theme) => {
                self.selected_theme = theme;
                let app_settings_path = get_app_settings_path();
                let settings = serde_json::json!({"theme": self.selected_theme});
                debug!("Writing settings to {}: {}", app_settings_path.to_str().unwrap() , settings);
                std::fs::write(app_settings_path, settings.to_string()).ok();
                Task::none()
            }
            Message::CopyToClipboard(data) => {
                iced::clipboard::write(data)
            }
            Message::UIMessage(ui_message) => {
                match ui_message {
                    UIMessage::NoOp => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(
                            wait_for_message(ui_rx),
                            |msg| msg,
                        );
                        wait_task
                    }
                    UIMessage::OpenWindow => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(
                            wait_for_message(ui_rx),
                            |msg| msg,
                        );
                        debug!("Opening main window...");
                        if let Some(window_id) = self.window {
                            Task::batch(vec![
                                window::gain_focus(window_id),
                                wait_task,
                            ])
                        } else {
                            let mut settings = window::Settings::default();
                            settings.min_size = Some(Size::new(400.0, 300.0));
                            settings.icon = window::icon::from_file("../../assets/icon.png").ok();
                            let (new_window_task, open_task) = window::open(settings);
                            self.window = Some(new_window_task);
                            Task::batch(vec![
                                open_task.map(Message::WindowOpened),
                                wait_task,
                            ])
                        }
                    }
                    UIMessage::DeviceConnected(mac) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(
                            wait_for_message(ui_rx),
                            |msg| msg,
                        );
                        debug!("Device connected: {}. Adding to connected devices list", mac);
                        let mut already_connected = false;
                        for device in &self.bluetooth_state.connected_devices {
                            if device == &mac {
                                already_connected = true;
                                break;
                            }
                        }
                        if !already_connected {
                            self.bluetooth_state.connected_devices.push(mac.clone());
                        }

                        Task::batch(vec![
                            wait_task,
                        ])
                    }
                    UIMessage::DeviceDisconnected(mac) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(
                            wait_for_message(ui_rx),
                            |msg| msg,
                        );
                        debug!("Device disconnected: {}", mac);
                        Task::batch(vec![
                            wait_task,
                        ])
                    }
                    UIMessage::AACPUIEvent(mac, event) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(
                            wait_for_message(ui_rx),
                            |msg| msg,
                        );
                        debug!("AACP UI Event for {}: {:?}", mac, event);
                        Task::batch(vec![
                            wait_task,
                        ])
                    }
                }
            }
        }
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let devices_json = std::fs::read_to_string(get_devices_path()).unwrap_or_else(|e| {
            error!("Failed to read devices file: {}", e);
            "{}".to_string()
        });
        let devices_list: HashMap<String, DeviceData> = serde_json::from_str(&devices_json).unwrap_or_else(|e| {
            error!("Deserialization failed: {}", e);
            HashMap::new()
        });
        let pane_grid = pane_grid::PaneGrid::new(&self.panes, |_pane_id, pane, _is_maximized| {
            match pane {
                Pane::Sidebar => {
                    let create_tab_button = |tab: Tab, label: &str, description: &str, connected: bool| -> Element<'_, Message> {
                        let label = label.to_string();
                        let is_selected = self.selected_tab == tab;
                        let col = column![
                            text(label).size(16),
                            text(
                                if connected {
                                    format!("Connected - {}", description)
                                } else {
                                    format!("{}", description)
                                }
                            ).size(12)
                        ];
                        let content = container(col)
                            .padding(8);
                        let style = move |theme: &Theme, _status| {
                            if is_selected {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary);
                                let mut border = Border::default();
                                border.color = theme.palette().text;
                                style.border = border.rounded(12);
                                style
                            } else {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary.scale_alpha(0.1));
                                let mut border = Border::default();
                                border.color = theme.palette().primary.scale_alpha(0.1);
                                style.border = border.rounded(8);
                                style.text_color = theme.palette().text;
                                style
                            }
                        };
                        button(content)
                            .style(style)
                            .padding(5)
                            .on_press(Message::SelectTab(tab))
                            .width(Length::Fill)
                            .into()
                    };

                    let create_settings_button = || -> Element<'_, Message> {
                        let label = "Settings".to_string();
                        let is_selected = self.selected_tab == Tab::Settings;
                        let col = column![text(label).size(16)];
                        let content = container(col)
                            .padding(8);
                        let style = move |theme: &Theme, _status| {
                            if is_selected {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary);
                                let mut border = Border::default();
                                border.color = theme.palette().text;
                                style.border = border.rounded(12);
                                style
                            } else {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary.scale_alpha(0.1));
                                let mut border = Border::default();
                                border.color = theme.palette().primary.scale_alpha(0.1);
                                style.border = border.rounded(8);
                                style.text_color = theme.palette().text;
                                style
                            }
                        };
                        button(content)
                            .style(style)
                            .padding(5)
                            .on_press(Message::SelectTab(Tab::Settings))
                            .width(Length::Fill)
                            .into()
                    };

                    let mut devices = column!().spacing(4);
                    let mut devices_vec: Vec<(String, DeviceData)> = devices_list.clone().into_iter().collect();
                    devices_vec.sort_by(|a, b| a.1.name.cmp(&b.1.name));
                    for (mac, device) in devices_vec {
                        let name = device.name.clone();
                        let tab_button = create_tab_button(
                            Tab::Device(mac.clone()),
                            &name,
                            &mac,
                            self.bluetooth_state.connected_devices.contains(&mac)
                        );
                        devices = devices.push(tab_button);
                    }

                    let settings = create_settings_button();

                    let content = column![
                        devices,
                        Space::with_height(Length::Fill),
                        settings
                    ]
                        .padding(12);

                    pane_grid::Content::new(content)
                }
                
                Pane::Content => {
                    let content = match &self.selected_tab {
                        Tab::Device(id) => {
                            if id == "none" {
                                container(
                                    text("Select a device".to_string()).size(16)
                                )
                                    .center_x(Length::Fill)
                                    .center_y(Length::Fill)
                            } else {
                                let mut information_col = column![];

                                let device_type = devices_list.get(id)
                                    .map(|d| d.type_.clone()).unwrap();

                                if device_type == DeviceType::AirPods {
                                    let device_information = devices_list.get(id)
                                        .and_then(|d| d.information.clone());
                                    match device_information {
                                        Some(DeviceInformation::AirPods(ref airpods_information)) => {
                                            information_col = information_col
                                                .push(text("Device Information").size(18).style(
                                                    |theme: &Theme| {
                                                        let mut style = text::Style::default();
                                                        style.color = Some(theme.palette().primary);
                                                        style
                                                    }
                                                ))
                                                .push(Space::with_height(Length::from(10)))
                                                .push(
                                                    row![
                                                        text("Model Number").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        text(airpods_information.model_number.clone()).size(16)
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Manufacturer").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        text(airpods_information.manufacturer.clone()).size(16)
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Serial Number").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        button(
                                                            text(
                                                                airpods_information.serial_number.clone()
                                                            )
                                                            .size(16)
                                                        )
                                                            .style(
                                                                |theme: &Theme, _status| {
                                                                    let mut style = Style::default();
                                                                    style.text_color = theme.palette().text;
                                                                    style.background = Some(Background::Color(Color::TRANSPARENT));
                                                                    style
                                                                }
                                                            )
                                                            .padding(0)
                                                            .on_press(Message::CopyToClipboard(airpods_information.serial_number.clone()))
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Left Serial Number").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        button(
                                                            text(
                                                                airpods_information.left_serial_number.clone()
                                                            )
                                                            .size(16)
                                                        )
                                                            .style(
                                                                |theme: &Theme, _status| {
                                                                    let mut style = Style::default();
                                                                    style.text_color = theme.palette().text;
                                                                    style.background = Some(Background::Color(Color::TRANSPARENT));
                                                                    style
                                                                }
                                                            )
                                                            .padding(0)
                                                            .on_press(Message::CopyToClipboard(airpods_information.left_serial_number.clone()))
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Right Serial Number").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        button(
                                                            text(
                                                                airpods_information.right_serial_number.clone()
                                                            )
                                                            .size(16)
                                                        )
                                                            .style(
                                                                |theme: &Theme, _status| {
                                                                    let mut style = Style::default();
                                                                    style.text_color = theme.palette().text;
                                                                    style.background = Some(Background::Color(Color::TRANSPARENT));
                                                                    style
                                                                }
                                                            )
                                                            .padding(0)
                                                            .on_press(Message::CopyToClipboard(airpods_information.right_serial_number.clone()))
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Version 1").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        text(airpods_information.version1.clone()).size(16)
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Version 2").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        text(airpods_information.version2.clone()).size(16)
                                                    ]
                                                )
                                                .push(
                                                    row![
                                                        text("Version 3").size(16).style(
                                                            |theme: &Theme| {
                                                                let mut style = text::Style::default();
                                                                style.color = Some(theme.palette().text);
                                                                style
                                                            }
                                                        ),
                                                        Space::with_width(Length::Fill),
                                                        text(airpods_information.version3.clone()).size(16)
                                                    ]
                                                );
                                            debug!("AirPods Information: {:?}", airpods_information);
                                        }
                                        _ => {
                                            error!("Expected AirPodsInformation, got something else: {:?}", device_information);
                                        },
                                    }
                                }
                                container(
                                    column![
                                        container(information_col)
                                            .style(
                                                |theme: &Theme| {
                                                    let mut style = container::Style::default();
                                                    style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                    let mut border = Border::default();
                                                    border.color = theme.palette().text;
                                                    style.border = border.rounded(20);
                                                    style
                                                }
                                            )
                                            .padding(20)
                                    ]
                                )
                                    .padding(20)
                                    .center_x(Length::Fill)
                                    .height(Length::Fill)
                            }
                        }
                        Tab::Settings => {
                            container(
                                column![
                                    text("Settings").size(40),
                                    Space::with_height(Length::from(20)),
                                    row![
                                        text("Theme:")
                                            .size(16),
                                        Space::with_width(Length::from(10)),
                                        combo_box(
                                            &self.theme_state,
                                            "Select theme",
                                            Some(&self.selected_theme),
                                            Message::ThemeSelected
                                        )
                                        .input_style(
                                            |theme: &Theme, _status| {
                                                text_input::Style {
                                                    background: Background::Color(Color::TRANSPARENT),
                                                    border: Border {
                                                        width: 0.5,
                                                        color: theme.palette().text,
                                                        radius: Radius::from(10.0),
                                                    },
                                                    icon: Default::default(),
                                                    placeholder: theme.palette().text.scale_alpha(0.5),
                                                    value: theme.palette().text,
                                                    selection: theme.palette().primary
                                                }
                                            }
                                        )
                                        .menu_style(
                                            |theme: &Theme| {
                                                menu::Style {
                                                    background: Background::Color(Color::TRANSPARENT),
                                                    border: Border {
                                                        width: 0.5,
                                                        color: theme.palette().text,
                                                        radius: Radius::from(10.0)
                                                    },
                                                    text_color: theme.palette().text,
                                                    selected_text_color: theme.palette().text,
                                                    selected_background: Background::Color(theme.palette().primary.scale_alpha(0.3)),
                                                }
                                            }
                                        )
                                        .width(Length::Fill)
                                    ]
                                    .align_y(Center)
                                ]
                            )
                                .padding(20)
                                .width(Length::Fill)
                                .height(Length::Fill)
                        },
                    };

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
        self.selected_theme.into()
    }

    fn subscription(&self) -> Subscription<Message> {
        window::close_events().map(Message::WindowClosed)
    }
}

async fn wait_for_message(
    ui_rx: Arc<Mutex<UnboundedReceiver<UIMessage>>>,
) -> Message {
    let mut rx = ui_rx.lock().await;
    match rx.recv().await {
        Some(msg) => Message::UIMessage(msg),
        None => {
            error!("UI message channel closed");
            Message::UIMessage(UIMessage::NoOp)
        }
    }
}