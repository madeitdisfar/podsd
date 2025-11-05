use crate::bluetooth::aacp::{AACPEvent, ControlCommandIdentifiers};

#[derive(Debug, Clone)]
pub enum UIMessage {
    OpenWindow,
    DeviceConnected(String),
    DeviceDisconnected(String),
    AACPUIEvent(String, AACPEvent),
    NoOp,
}

#[derive(Debug, Clone)]
pub enum UICommand {
    SetControlCommandStatus(String, ControlCommandIdentifiers, Vec<u8>),
}