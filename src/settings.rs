//! The AirPods settings podsd exposes, and how each maps onto an AACP control
//! command. Clients read the table through `DescribeSettings`, so a setting added
//! here shows up in every client without changes there.
//!
//! Encodings follow LibrePods' Android app (AirPodsViewModel / *SettingsScreen):
//! booleans are 0x01 on / 0x02 off.
//!
//! The AirPods report every setting when the control channel opens, but don't
//! confirm a change (only listening mode changes come back), so a setting is
//! taken as changed once the write is sent, as the Android app does.

use crate::bluetooth::aacp::ControlCommandIdentifiers as Id;
use crate::bluetooth::att::ATTHandles;
use serde::{Deserialize, Serialize};

/// Settings the AirPods take but never report, not even when connecting (seen
/// on AirPods Pro 3). podsd remembers the last value set per pair; until one is
/// set, the default here (None: unknown).
pub const UNREPORTED: &[(&str, Option<bool>)] = &[
    ("allow-off-mode", None),
    // On from the factory
    ("ear-detection", Some(true)),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    Bool(bool),
    Str(String),
    Int(i32),
}

pub enum Kind {
    Bool,
    /// (raw value, name). Several raw values may share a name; writing uses the first.
    Choice(&'static [(u8, &'static str)]),
    Int { min: i32, max: i32 },
}

/// Where a setting lives on the AirPods
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Via {
    /// An AACP control command: reported when the channel opens. Booleans are
    /// 0x01 on / 0x02 off.
    Control(Id),
    /// An ATT handle (bluetooth/att.rs): read by podsd, at connect and on
    /// Refresh; a change made on another device isn't announced. Booleans
    /// are 0x01 on / 0x00 off.
    Att(ATTHandles),
}

pub struct SettingDef {
    pub key: &'static str,
    pub via: Via,
    pub kind: Kind,
    pub label: &'static str,
}

/// In the order iOS's AirPods settings show them.
pub const LISTENING_MODES: &[(u8, &str)] = &[
    (0x01, "off"),
    (0x03, "transparency"),
    (0x04, "adaptive"),
    (0x02, "noise-cancellation"),
];

const SPEEDS: &[(u8, &str)] = &[(0x00, "default"), (0x01, "slower"), (0x02, "slowest")];
// Press and hold duration, named as iOS names it
const DURATIONS: &[(u8, &str)] = &[(0x00, "default"), (0x01, "shorter"), (0x02, "shortest")];

// Android sends 1 for "default"; AirPods Pro 3 report 0 until it's changed.
const SWIPE_LENGTHS: &[(u8, &str)] = &[
    (0x01, "default"),
    (0x02, "longer"),
    (0x03, "longest"),
    (0x00, "default"),
];

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "listening-mode",
        via: Via::Control(Id::ListeningMode),
        kind: Kind::Choice(LISTENING_MODES),
        label: "Listening mode",
    },
    SettingDef {
        key: "allow-off-mode",
        via: Via::Control(Id::AllowOffOption),
        kind: Kind::Bool,
        label: "Off listening mode",
    },
    SettingDef {
        key: "conversation-awareness",
        via: Via::Control(Id::ConversationDetectConfig),
        kind: Kind::Bool,
        label: "Conversation Awareness",
    },
    SettingDef {
        key: "personalized-volume",
        via: Via::Control(Id::AdaptiveVolumeConfig),
        kind: Kind::Bool,
        label: "Personalized Volume",
    },
    SettingDef {
        key: "adaptive-audio-level",
        via: Via::Control(Id::AutoAncStrength),
        kind: Kind::Int { min: 0, max: 100 },
        label: "Adaptive Audio level",
    },
    SettingDef {
        key: "one-bud-anc",
        via: Via::Control(Id::OneBudAncMode),
        kind: Kind::Bool,
        label: "Noise Cancellation with one AirPod",
    },
    SettingDef {
        key: "loud-sound-reduction",
        via: Via::Att(ATTHandles::LoudSoundReduction),
        kind: Kind::Bool,
        label: "Loud Sound Reduction",
    },
    SettingDef {
        key: "ear-detection",
        via: Via::Control(Id::EarDetectionConfig),
        kind: Kind::Bool,
        label: "Automatic ear detection",
    },
    SettingDef {
        key: "sleep-detection",
        via: Via::Control(Id::SleepDetectionConfig),
        kind: Kind::Bool,
        label: "Pause media when falling asleep",
    },
    SettingDef {
        key: "volume-swipe",
        via: Via::Control(Id::VolumeSwipeMode),
        kind: Kind::Bool,
        label: "Volume control (swipe)",
    },
    SettingDef {
        key: "volume-swipe-length",
        via: Via::Control(Id::VolumeSwipeInterval),
        kind: Kind::Choice(SWIPE_LENGTHS),
        label: "Volume swipe length",
    },
    SettingDef {
        key: "press-speed",
        via: Via::Control(Id::DoubleClickInterval),
        kind: Kind::Choice(SPEEDS),
        label: "Press speed",
    },
    SettingDef {
        key: "press-hold-duration",
        via: Via::Control(Id::ClickHoldInterval),
        kind: Kind::Choice(DURATIONS),
        label: "Press and hold duration",
    },
];

pub fn by_key(key: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.key == key)
}

pub fn by_id(id: Id) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.via == Via::Control(id))
}

/// The settings podsd reads over ATT
pub fn over_att() -> impl Iterator<Item = (&'static SettingDef, ATTHandles)> {
    SETTINGS.iter().filter_map(|d| match d.via {
        Via::Att(handle) => Some((d, handle)),
        Via::Control(_) => None,
    })
}

impl SettingDef {
    /// A boolean's (on, off) bytes
    fn bool_bytes(&self) -> (u8, u8) {
        match self.via {
            Via::Control(_) => (0x01, 0x02),
            Via::Att(_) => (0x01, 0x00),
        }
    }

    /// The value the AirPods reported.
    pub fn decode(&self, raw: &[u8]) -> Option<SettingValue> {
        let first = *raw.first()?;
        let (on, off) = self.bool_bytes();
        match self.kind {
            Kind::Bool if first == on => Some(SettingValue::Bool(true)),
            Kind::Bool if first == off => Some(SettingValue::Bool(false)),
            Kind::Bool => None,
            Kind::Choice(choices) => choices
                .iter()
                .find(|(v, _)| *v == first)
                .map(|(_, name)| SettingValue::Str(name.to_string())),
            Kind::Int { .. } => Some(SettingValue::Int(first as i32)),
        }
    }

    pub fn encode(&self, value: &SettingValue) -> Result<Vec<u8>, String> {
        match (&self.kind, value) {
            (Kind::Bool, SettingValue::Bool(b)) => {
                let (on, off) = self.bool_bytes();
                Ok(vec![if *b { on } else { off }])
            }
            (Kind::Choice(choices), SettingValue::Str(s)) => choices
                .iter()
                .find(|(_, name)| name == s)
                .map(|(v, _)| vec![*v])
                .ok_or_else(|| {
                    format!("{}: expected one of {}", self.key, self.choice_names().join(", "))
                }),
            (Kind::Int { min, max }, SettingValue::Int(i)) => {
                if i < min || i > max {
                    Err(format!("{}: {} is outside {}..{}", self.key, i, min, max))
                } else {
                    Ok(vec![*i as u8])
                }
            }
            _ => Err(format!("{}: expected a {}", self.key, self.type_name())),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self.kind {
            Kind::Bool => "bool",
            Kind::Choice(_) => "choice",
            Kind::Int { .. } => "int",
        }
    }

    pub fn choice_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        if let Kind::Choice(choices) = self.kind {
            for (_, name) in choices {
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    pub fn range(&self) -> (i32, i32) {
        match self.kind {
            Kind::Int { min, max } => (min, max),
            _ => (0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans_use_each_channels_bytes() {
        let aacp = by_key("conversation-awareness").unwrap();
        assert_eq!(aacp.encode(&SettingValue::Bool(false)).unwrap(), [0x02]);
        assert_eq!(aacp.decode(&[0x02]), Some(SettingValue::Bool(false)));
        let att = by_key("loud-sound-reduction").unwrap();
        assert_eq!(att.via, Via::Att(ATTHandles::LoudSoundReduction));
        assert_eq!(att.encode(&SettingValue::Bool(true)).unwrap(), [0x01]);
        assert_eq!(att.encode(&SettingValue::Bool(false)).unwrap(), [0x00]);
        assert_eq!(att.decode(&[0x01]), Some(SettingValue::Bool(true)));
        assert_eq!(att.decode(&[0x00]), Some(SettingValue::Bool(false)));
        // 02 means nothing over ATT
        assert_eq!(att.decode(&[0x02]), None);
    }

    #[test]
    fn only_att_settings_are_read_over_att() {
        let keys: Vec<_> = over_att().map(|(d, _)| d.key).collect();
        assert_eq!(keys, ["loud-sound-reduction"]);
        assert!(by_id(Id::ConversationDetectConfig).is_some());
    }
}
