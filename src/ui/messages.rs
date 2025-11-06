use crate::bluetooth::aacp::{AACPEvent, ControlCommandIdentifiers};
use crate::devices::enums::NothingAncMode;

#[derive(Debug, Clone)]
pub enum BluetoothUIMessage {
    OpenWindow,
    DeviceConnected(String), // mac
    DeviceDisconnected(String), // mac
    AACPUIEvent(String, AACPEvent), // mac, event
    ATTNotification(String, u16, Vec<u8>), // mac, handle, data
    NoOp
}

#[derive(Debug, Clone)]
pub enum UICommand {
    AirPods(AirPodsCommand),
    Nothing(NothingCommand),
}

#[derive(Debug, Clone)]
pub enum AirPodsCommand {
    SetControlCommandStatus(String, ControlCommandIdentifiers, Vec<u8>),
    RenameDevice(String, String),
}

#[derive(Debug, Clone)]
pub enum NothingCommand {
    SetNoiseCancellationMode(String, NothingAncMode),
}

