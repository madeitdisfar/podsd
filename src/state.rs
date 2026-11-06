//! What podsd knows about one pair of AirPods. The D-Bus properties are views of
//! this struct, and a change is announced by diffing two of its property lists.

use crate::settings::SettingValue;
use std::collections::BTreeMap;
use zbus::zvariant::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Battery {
    /// Percent, or -1 when unknown or that part isn't there.
    pub level: i32,
    pub charging: bool,
    /// Read just now (false: the last reading, kept; used for the case, which
    /// only reports while a bud is in it)
    pub live: bool,
    /// When `level` was read (Unix seconds; 0 unknown)
    pub time: i64,
}

impl Default for Battery {
    fn default() -> Self {
        Battery { level: -1, charging: false, live: false, time: 0 }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Battery {
    pub fn reading(level: i32, charging: bool) -> Self {
        Battery { level, charging, live: true, time: now() }
    }
    /// The same level, no longer live
    pub fn kept(&self) -> Self {
        Battery { charging: false, live: false, ..self.clone() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceState {
    pub address: String,
    pub name: String,
    pub model: String,
    pub firmware: String,
    /// BlueZ has the device connected.
    pub connected: bool,
    /// The AACP control channel is up: settings and live state are available.
    pub ready: bool,
    pub battery_left: Battery,
    pub battery_right: Battery,
    pub battery_case: Battery,
    /// AirPods Max report one battery for the whole headset.
    pub battery_headphone: Battery,
    /// "aacp" (connected here), "ble" (from adverts while connected elsewhere) or "".
    pub battery_source: String,
    /// "in-ear", "out-of-ear", "in-case", "disconnected" or "" (unknown).
    pub ear_primary: String,
    pub ear_secondary: String,
    pub listening_mode: String,
    /// This computer owns the AirPods' audio (vs. another device such as a phone).
    pub owns_audio: bool,
    pub settings: BTreeMap<String, SettingValue>,
    /// The case's lid is open nearby (from BLE adverts)
    pub case_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    S(String),
    B(bool),
    I(i32),
    X(i64),
    Strs(Vec<String>),
    Settings(BTreeMap<String, SettingValue>),
}

impl Prop {
    pub fn to_value(&self) -> Value<'static> {
        match self {
            Prop::S(s) => Value::from(s.clone()),
            Prop::B(b) => Value::from(*b),
            Prop::I(i) => Value::from(*i),
            Prop::X(x) => Value::from(*x),
            Prop::Strs(v) => Value::from(v.clone()),
            Prop::Settings(map) => Value::from(settings_dict(map)),
        }
    }
}

pub fn setting_to_value(v: &SettingValue) -> Value<'static> {
    match v {
        SettingValue::Bool(b) => Value::from(*b),
        SettingValue::Str(s) => Value::from(s.clone()),
        SettingValue::Int(i) => Value::from(*i),
    }
}

pub fn settings_dict(map: &BTreeMap<String, SettingValue>) -> std::collections::HashMap<String, Value<'static>> {
    map.iter()
        .map(|(k, v)| (k.clone(), setting_to_value(v)))
        .collect()
}

impl DeviceState {
    pub fn new(address: &str, name: &str) -> Self {
        DeviceState {
            address: address.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// The listening modes the AirPods take now, in iOS's order. Off only with
    /// the "Off listening mode" setting on. With one bud in an ear, noise
    /// cancellation and Adaptive need the "Noise Cancellation with one AirPod"
    /// setting (as on iOS: otherwise only Off and Transparency).
    pub fn listening_modes(&self) -> Vec<String> {
        let on = |key: &str| self.settings.get(key) == Some(&SettingValue::Bool(true));
        let one_bud = [&self.ear_primary, &self.ear_secondary]
            .iter()
            .filter(|e| e.as_str() == "in-ear")
            .count()
            == 1;
        crate::settings::LISTENING_MODES
            .iter()
            .map(|(_, name)| *name)
            .filter(|m| match *m {
                "off" => on("allow-off-mode"),
                "noise-cancellation" | "adaptive" => !one_bud || on("one-bud-anc"),
                _ => true,
            })
            .map(String::from)
            .collect()
    }

    pub fn in_ear(&self) -> bool {
        self.ear_primary == "in-ear" || self.ear_secondary == "in-ear"
    }

    /// Forget everything that only holds while the control channel is up.
    pub fn clear_live(&mut self) {
        self.ready = false;
        self.battery_left = Battery::default();
        self.battery_right = Battery::default();
        // The case keeps its last reading
        self.battery_case = self.battery_case.kept();
        self.battery_headphone = Battery::default();
        self.battery_source.clear();
        self.ear_primary.clear();
        self.ear_secondary.clear();
        self.listening_mode.clear();
        self.owns_audio = false;
        self.settings.clear();
    }

    /// Every D-Bus property, by its D-Bus name.
    pub fn props(&self) -> Vec<(&'static str, Prop)> {
        vec![
            ("Address", Prop::S(self.address.clone())),
            ("Name", Prop::S(self.name.clone())),
            ("Model", Prop::S(self.model.clone())),
            ("Firmware", Prop::S(self.firmware.clone())),
            ("Connected", Prop::B(self.connected)),
            ("Ready", Prop::B(self.ready)),
            ("BatteryLeft", Prop::I(self.battery_left.level)),
            ("BatteryRight", Prop::I(self.battery_right.level)),
            ("BatteryCase", Prop::I(self.battery_case.level)),
            ("BatteryHeadphone", Prop::I(self.battery_headphone.level)),
            ("ChargingLeft", Prop::B(self.battery_left.charging)),
            ("ChargingRight", Prop::B(self.battery_right.charging)),
            ("ChargingCase", Prop::B(self.battery_case.charging)),
            ("ChargingHeadphone", Prop::B(self.battery_headphone.charging)),
            ("BatteryCaseLive", Prop::B(self.battery_case.live)),
            ("BatteryCaseTime", Prop::X(self.battery_case.time)),
            ("CaseOpen", Prop::B(self.case_open)),
            ("BatterySource", Prop::S(self.battery_source.clone())),
            ("EarPrimary", Prop::S(self.ear_primary.clone())),
            ("EarSecondary", Prop::S(self.ear_secondary.clone())),
            ("InEar", Prop::B(self.in_ear())),
            ("ListeningMode", Prop::S(self.listening_mode.clone())),
            ("ListeningModes", Prop::Strs(self.listening_modes())),
            ("OwnsAudio", Prop::B(self.owns_audio)),
            ("Settings", Prop::Settings(self.settings.clone())),
        ]
    }
}

/// The properties that differ between two states.
pub fn diff(old: &DeviceState, new: &DeviceState) -> Vec<(&'static str, Prop)> {
    old.props()
        .into_iter()
        .zip(new.props())
        .filter(|((_, a), (_, b))| a != b)
        .map(|(_, changed)| changed)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pods(primary: &str, secondary: &str, one_bud_anc: bool, allow_off: bool) -> DeviceState {
        let mut s = DeviceState::new("00:00:00:00:00:00", "test");
        s.ear_primary = primary.into();
        s.ear_secondary = secondary.into();
        s.settings.insert("one-bud-anc".into(), SettingValue::Bool(one_bud_anc));
        s.settings.insert("allow-off-mode".into(), SettingValue::Bool(allow_off));
        s
    }

    #[test]
    fn both_in_ears_offers_every_mode() {
        assert_eq!(
            pods("in-ear", "in-ear", false, true).listening_modes(),
            ["off", "transparency", "adaptive", "noise-cancellation"]
        );
    }

    #[test]
    fn one_bud_without_one_bud_anc_offers_off_and_transparency() {
        assert_eq!(pods("in-ear", "in-case", false, true).listening_modes(), ["off", "transparency"]);
        assert_eq!(pods("out-of-ear", "in-ear", false, false).listening_modes(), ["transparency"]);
    }

    #[test]
    fn one_bud_with_one_bud_anc_offers_every_mode() {
        assert_eq!(
            pods("in-ear", "out-of-ear", true, true).listening_modes(),
            ["off", "transparency", "adaptive", "noise-cancellation"]
        );
    }
}
