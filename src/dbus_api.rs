//! The D-Bus interfaces. Properties are read-only views of `DeviceState`; changes
//! are requested with methods, and the new values arrive as PropertiesChanged once
//! the AirPods confirm them.

use crate::bluetooth::aacp::ControlCommandIdentifiers;
use crate::hub::Hub;
use crate::settings::{self, LISTENING_MODES, SETTINGS, SettingValue};
use crate::state::{DeviceState, settings_dict};
use log::info;
use std::collections::HashMap;
use zbus::fdo;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

pub struct ManagerIface;

#[interface(name = "io.github.madeitdisfar.Podsd.Manager1")]
impl ManagerIface {
    #[zbus(property)]
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

pub struct DeviceIface {
    hub: Hub,
    address: String,
}

impl DeviceIface {
    pub fn new(hub: Hub, address: String) -> Self {
        DeviceIface { hub, address }
    }

    async fn st(&self) -> DeviceState {
        self.hub.state(&self.address).await.unwrap_or_default()
    }

    async fn send(&self, id: ControlCommandIdentifiers, value: &[u8]) -> fdo::Result<()> {
        let aacp = self
            .hub
            .aacp(&self.address)
            .await
            .ok_or_else(|| fdo::Error::Failed("AirPods are not connected".into()))?;
        aacp.send_control_command(id, value)
            .await
            .map_err(|e| fdo::Error::Failed(format!("Sending to the AirPods failed: {e}")))
    }

    async fn bluez_call(&self, method: &str) -> fdo::Result<()> {
        let path = self
            .hub
            .bluez_path(&self.address)
            .await
            .ok_or_else(|| fdo::Error::Failed("BlueZ doesn't know this device".into()))?;
        self.hub
            .system()
            .call_method(Some("org.bluez"), path.as_str(), Some("org.bluez.Device1"), method, &())
            .await
            .map(|_| ())
            .map_err(|e| fdo::Error::Failed(format!("BlueZ {method} failed: {e}")))
    }
}

fn to_setting_value(value: &Value<'_>) -> Option<SettingValue> {
    match value {
        Value::Bool(b) => Some(SettingValue::Bool(*b)),
        Value::Str(s) => Some(SettingValue::Str(s.to_string())),
        Value::I32(i) => Some(SettingValue::Int(*i)),
        Value::U8(i) => Some(SettingValue::Int(*i as i32)),
        Value::I16(i) => Some(SettingValue::Int(*i as i32)),
        Value::U16(i) => Some(SettingValue::Int(*i as i32)),
        Value::U32(i) => i32::try_from(*i).ok().map(SettingValue::Int),
        Value::I64(i) => i32::try_from(*i).ok().map(SettingValue::Int),
        Value::U64(i) => i32::try_from(*i).ok().map(SettingValue::Int),
        Value::Value(inner) => to_setting_value(inner),
        _ => None,
    }
}

#[interface(name = "io.github.madeitdisfar.Podsd.Device1")]
impl DeviceIface {
    #[zbus(property)]
    async fn address(&self) -> String {
        self.address.clone()
    }
    #[zbus(property)]
    async fn name(&self) -> String {
        self.st().await.name
    }
    #[zbus(property)]
    async fn model(&self) -> String {
        self.st().await.model
    }
    #[zbus(property)]
    async fn firmware(&self) -> String {
        self.st().await.firmware
    }
    #[zbus(property)]
    async fn connected(&self) -> bool {
        self.st().await.connected
    }
    #[zbus(property)]
    async fn ready(&self) -> bool {
        self.st().await.ready
    }
    #[zbus(property)]
    async fn battery_left(&self) -> i32 {
        self.st().await.battery_left.level
    }
    #[zbus(property)]
    async fn battery_right(&self) -> i32 {
        self.st().await.battery_right.level
    }
    #[zbus(property)]
    async fn battery_case(&self) -> i32 {
        self.st().await.battery_case.level
    }
    #[zbus(property)]
    async fn battery_headphone(&self) -> i32 {
        self.st().await.battery_headphone.level
    }
    #[zbus(property)]
    async fn charging_left(&self) -> bool {
        self.st().await.battery_left.charging
    }
    #[zbus(property)]
    async fn charging_right(&self) -> bool {
        self.st().await.battery_right.charging
    }
    #[zbus(property)]
    async fn charging_case(&self) -> bool {
        self.st().await.battery_case.charging
    }
    #[zbus(property)]
    async fn charging_headphone(&self) -> bool {
        self.st().await.battery_headphone.charging
    }
    #[zbus(property)]
    async fn battery_source(&self) -> String {
        self.st().await.battery_source
    }
    #[zbus(property)]
    async fn ear_primary(&self) -> String {
        self.st().await.ear_primary
    }
    #[zbus(property)]
    async fn ear_secondary(&self) -> String {
        self.st().await.ear_secondary
    }
    #[zbus(property)]
    async fn in_ear(&self) -> bool {
        self.st().await.in_ear()
    }
    #[zbus(property)]
    async fn listening_mode(&self) -> String {
        self.st().await.listening_mode
    }
    #[zbus(property)]
    async fn owns_audio(&self) -> bool {
        self.st().await.owns_audio
    }
    #[zbus(property)]
    async fn settings(&self) -> HashMap<String, Value<'static>> {
        settings_dict(&self.st().await.settings)
    }

    /// "off", "noise-cancellation", "transparency" or "adaptive". "off" needs the
    /// allow-off-mode setting on.
    async fn set_listening_mode(&self, mode: String) -> fdo::Result<()> {
        let raw = LISTENING_MODES
            .iter()
            .find(|(_, name)| *name == mode)
            .map(|(v, _)| *v)
            .ok_or_else(|| {
                fdo::Error::InvalidArgs(format!(
                    "Unknown listening mode {mode:?}; expected one of {}",
                    LISTENING_MODES.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
                ))
            })?;
        info!("{}: listening mode -> {}", self.address, mode);
        self.send(ControlCommandIdentifiers::ListeningMode, &[raw]).await
    }

    /// Sets one of the settings listed by DescribeSettings.
    async fn set_setting(&self, key: String, value: OwnedValue) -> fdo::Result<()> {
        let def = settings::by_key(&key)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("Unknown setting {key:?}")))?;
        let value = to_setting_value(&value)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("{key}: unsupported value type")))?;
        let raw = def.encode(&value).map_err(fdo::Error::InvalidArgs)?;
        info!("{}: {} -> {:?}", self.address, key, value);
        self.send(def.id, &raw).await?;
        // The AirPods don't confirm settings, only listening mode changes (which
        // they may refuse, e.g. Off while it isn't allowed): take the rest as set.
        if def.id != ControlCommandIdentifiers::ListeningMode {
            if settings::UNREPORTED.iter().any(|(k, _)| *k == key) {
                crate::utils::remember_setting(&self.address, &key, &value);
            }
            self.hub
                .update(&self.address, |s| {
                    s.settings.insert(key, value);
                })
                .await;
        }
        Ok(())
    }

    /// Every setting: (key, type "bool" | "choice" | "int", label, choices, min, max).
    async fn describe_settings(&self) -> Vec<(String, String, String, Vec<String>, i32, i32)> {
        SETTINGS
            .iter()
            .map(|d| {
                let (min, max) = d.range();
                (
                    d.key.to_string(),
                    d.type_name().to_string(),
                    d.label.to_string(),
                    d.choice_names(),
                    min,
                    max,
                )
            })
            .collect()
    }

    /// Renames the AirPods (as shown on every device they pair with).
    async fn rename(&self, name: String) -> fdo::Result<()> {
        if name.is_empty() || name.len() > 32 {
            return Err(fdo::Error::InvalidArgs("Name must be 1 to 32 bytes".into()));
        }
        let aacp = self
            .hub
            .aacp(&self.address)
            .await
            .ok_or_else(|| fdo::Error::Failed("AirPods are not connected".into()))?;
        aacp.send_rename_packet(&name)
            .await
            .map_err(|e| fdo::Error::Failed(format!("Rename failed: {e}")))?;
        self.hub.update(&self.address, |s| s.name = name).await;
        Ok(())
    }

    /// Asks BlueZ to connect the AirPods.
    async fn connect(&self) -> fdo::Result<()> {
        self.bluez_call("Connect").await
    }

    /// Asks BlueZ to disconnect the AirPods.
    async fn disconnect(&self) -> fdo::Result<()> {
        self.bluez_call("Disconnect").await
    }

    /// A stem press: press "single" | "double" | "triple" | "long", bud "left" | "right".
    /// The AirPods only report presses podsd asked for (stemControl in the config).
    #[zbus(signal)]
    pub async fn stem_press(emitter: &SignalEmitter<'_>, press: &str, bud: &str) -> zbus::Result<()>;
}
