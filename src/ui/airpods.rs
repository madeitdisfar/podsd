use std::collections::HashMap;
use iced::widget::{button, column, container, row, text, toggler, Space};
use iced::{Background, Border, Color, Length, Theme};
use iced::widget::button::Style;
use log::error;
use crate::devices::enums::{AirPodsState, DeviceData, DeviceInformation};
use crate::ui::window::{DeviceMessage, Message};

pub fn airpods_view<'a>(
    mac: &str,
    devices_list: &HashMap<String, DeviceData>,
    state: &AirPodsState,
) -> iced::widget::Container<'a, Message> {
    let mut information_col = column![];
    let mac = mac.to_string();
    if let Some(device) = devices_list.get(mac.as_str()) {
        if let Some(DeviceInformation::AirPods(ref airpods_info)) = device.information {
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
                        text(airpods_info.model_number.clone()).size(16)
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
                        text(airpods_info.manufacturer.clone()).size(16)
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
                            text(airpods_info.serial_number.clone()).size(16)
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
                            .on_press(Message::CopyToClipboard(airpods_info.serial_number.clone()))
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
                            text(airpods_info.left_serial_number.clone()).size(16)
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
                            .on_press(Message::CopyToClipboard(airpods_info.left_serial_number.clone()))
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
                            text(airpods_info.right_serial_number.clone()).size(16)
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
                            .on_press(Message::CopyToClipboard(airpods_info.right_serial_number.clone()))
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
                        text(airpods_info.version1.clone()).size(16)
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
                        text(airpods_info.version2.clone()).size(16)
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
                        text(airpods_info.version3.clone()).size(16)
                    ]
                );
        } else {
            error!("Expected AirPodsInformation for device {}, got something else", mac);
        }
    }

    let toggler_widget = toggler(state.conversation_awareness_enabled)
        .label("Conversation Awareness")
        .on_toggle(move |is_enabled| Message::DeviceMessage(mac.to_string(), DeviceMessage::ConversationAwarenessToggled(is_enabled)));

    container(
        column![
            toggler_widget,
            Space::with_height(Length::from(10)),
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
