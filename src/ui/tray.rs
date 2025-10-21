// use ksni::TrayMethods; // provides the spawn method

use ab_glyph::{Font, ScaleFont};
use ksni::{Icon, ToolTip};

use crate::bluetooth::aacp::ControlCommandIdentifiers;

#[derive(Debug)]
pub(crate) struct MyTray {
    pub(crate) conversation_detect_enabled: Option<bool>,
    pub(crate) battery_l: Option<u8>,
    pub(crate) battery_l_status: Option<crate::bluetooth::aacp::BatteryStatus>,
    pub(crate) battery_r: Option<u8>,
    pub(crate) battery_r_status: Option<crate::bluetooth::aacp::BatteryStatus>,
    pub(crate) battery_c: Option<u8>,
    pub(crate) battery_c_status: Option<crate::bluetooth::aacp::BatteryStatus>,
    pub(crate) connected: bool,
    pub(crate) listening_mode: Option<u8>,
    pub(crate) allow_off_option: Option<u8>,
    pub(crate) command_tx: Option<tokio::sync::mpsc::UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>>,
}

impl ksni::Tray for MyTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }
    fn title(&self) -> String {
        "AirPods".into()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        // text to icon pixmap
        let text = if self.connected {
            let min_battery = match (self.battery_l, self.battery_r) {
                (Some(l), Some(r)) => Some(l.min(r)),
                (Some(l), None) => Some(l),
                (None, Some(r)) => Some(r),
                (None, None) => None,
            };
            min_battery.map(|b| format!("{}", b)).unwrap_or("?".to_string())
        } else {
            "D".into()
        };
        let icon = generate_icon(&text, true);
        vec![icon]
    }
    fn tool_tip(&self) -> ToolTip {
        if self.connected {
            let l = self.battery_l.map(|b| format!("L: {}%", b)).unwrap_or("L: ?".to_string());
            let l_status = self.battery_l_status.map(|s| format!(" ({:?})", s)).unwrap_or("".to_string());
            let r = self.battery_r.map(|b| format!("R: {}%", b)).unwrap_or("R: ?".to_string());
            let r_status = self.battery_r_status.map(|s| format!(" ({:?})", s)).unwrap_or("".to_string());
            let c = self.battery_c.map(|b| format!("C: {}%", b)).unwrap_or("C: ?".to_string());
            let c_status = self.battery_c_status.map(|s| format!(" ({:?})", s)).unwrap_or("".to_string());
            ToolTip {
                icon_name: "".to_string(),
                icon_pixmap: vec![],
                title: "Battery Status".to_string(),
                description: format!("{}{} {}{} {}{}", l, l_status, r, r_status, c, c_status),
            }
        } else {
            ToolTip {
                icon_name: "".to_string(),
                icon_pixmap: vec![],
                title: "Not Connected".to_string(),
                description: "Device is not connected.".to_string(),
            }
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let allow_off = self.allow_off_option == Some(0x01);
        let options = if allow_off {
            vec![
                ("Off", 0x01),
                ("ANC", 0x02),
                ("Transparency", 0x03),
                ("Adaptive", 0x04),
            ]
        } else {
            vec![
                ("ANC", 0x02),
                ("Transparency", 0x03),
                ("Adaptive", 0x04),
            ]
        };
        let selected = self.listening_mode.and_then(|mode| {
            options.iter().position(|&(_, val)| val == mode)
        }).unwrap_or(0);
        let options_clone = options.clone();
        vec![
            RadioGroup {
                selected,
                select: Box::new(move |this: &mut Self, current| {
                    if let Some(tx) = &this.command_tx {
                        let value = options_clone.get(current).map(|&(_, val)| val).unwrap_or(0x02);
                        let _ = tx.send((ControlCommandIdentifiers::ListeningMode, vec![value]));
                    }
                }),
                options: options.into_iter().map(|(label, _)| RadioItem {
                    label: label.into(),
                    ..Default::default()
                }).collect(),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Conversation Detection".into(),
                checked: self.conversation_detect_enabled.unwrap_or(false),
                enabled: self.conversation_detect_enabled.is_some(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(tx) = &this.command_tx {
                        if let Some(is_enabled) = this.conversation_detect_enabled {
                            let value = if is_enabled { 0x02 } else { 0x01 };
                            let _ = tx.send((ControlCommandIdentifiers::ConversationDetectConfig, vec![value]));
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),    
            StandardItem {
                label: "Exit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn generate_icon(text: &str, text_mode: bool) -> Icon {
    use ab_glyph::{FontRef, PxScale};
    use image::{ImageBuffer, Rgba};
    use imageproc::drawing::draw_text_mut;

    let width = 64;
    let height = 64;

    let mut img = ImageBuffer::from_fn(width, height, |_, _| Rgba([0u8, 0u8, 0u8, 0u8]));

    if !text_mode {
        let percentage = if text.ends_with('%') {
            text.trim_end_matches('%').parse::<f32>().unwrap_or(0.0) / 100.0
        } else {
            0.0
        };

        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0;
        let inner_radius = 22.0;
        let outer_radius = 28.0;

        // ring background
        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - center_x;
                let dy = y as f32 - center_y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > inner_radius && dist <= outer_radius {
                    img.put_pixel(x, y, Rgba([128u8, 128u8, 128u8, 255u8]));
                }
            }
        }

        // ring
        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - center_x;
                let dy = y as f32 - center_y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > inner_radius && dist <= outer_radius {
                    let angle = dy.atan2(dx);
                    let angle_from_top = (std::f32::consts::PI / 2.0 - angle).rem_euclid(2.0 * std::f32::consts::PI);
                    if angle_from_top <= percentage * 2.0 * std::f32::consts::PI {
                        img.put_pixel(x, y, Rgba([0u8, 255u8, 0u8, 255u8]));
                    }
                }
            }
        }
    } else {
        // battery text
        let font_data = include_bytes!("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        let font = match FontRef::try_from_slice(font_data) {
            Ok(f) => f,
            Err(_) => {
                return Icon {
                    width: width as i32,
                    height: height as i32,
                    data: vec![0u8; (width * height * 4) as usize],
                };
            }
        };

        let scale = PxScale::from(48.0);
        let color = Rgba([255u8, 255u8, 255u8, 255u8]);

        let scaled_font = font.as_scaled(scale);
        let mut text_width = 0.0;
        for c in text.chars() {
            let glyph_id = font.glyph_id(c);
            text_width += scaled_font.h_advance(glyph_id);
        }
        let x = ((width as f32 - text_width) / 2.0).max(0.0) as i32;
        let y = ((height as f32 - scale.y) / 2.0).max(0.0) as i32;

        draw_text_mut(&mut img, color, x, y, scale, &font, text);
    }

    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for pixel in img.pixels() {
        data.push(pixel[3]);
        data.push(pixel[0]);
        data.push(pixel[1]);
        data.push(pixel[2]);
    }

    Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}