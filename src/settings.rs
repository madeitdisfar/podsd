//! The AirPods settings podsd exposes, and how each maps onto an AACP control
//! command. Clients read the table through `DescribeSettings`, so a setting added
//! here shows up in every client without changes there.
//!
//! Encodings follow LibrePods' Android app (AirPodsViewModel / *SettingsScreen):
//! booleans are 0x01 on / 0x02 off.

use crate::bluetooth::aacp::ControlCommandIdentifiers as Id;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
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

pub struct SettingDef {
    pub key: &'static str,
    pub id: Id,
    pub kind: Kind,
    pub label: &'static str,
}

pub const LISTENING_MODES: &[(u8, &str)] = &[
    (0x01, "off"),
    (0x02, "noise-cancellation"),
    (0x03, "transparency"),
    (0x04, "adaptive"),
];

const SPEEDS: &[(u8, &str)] = &[(0x00, "default"), (0x01, "slower"), (0x02, "slowest")];

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
        id: Id::ListeningMode,
        kind: Kind::Choice(LISTENING_MODES),
        label: "Listening mode",
    },
    SettingDef {
        key: "allow-off-mode",
        id: Id::AllowOffOption,
        kind: Kind::Bool,
        label: "Off listening mode",
    },
    SettingDef {
        key: "conversation-awareness",
        id: Id::ConversationDetectConfig,
        kind: Kind::Bool,
        label: "Conversation Awareness",
    },
    SettingDef {
        key: "personalized-volume",
        id: Id::AdaptiveVolumeConfig,
        kind: Kind::Bool,
        label: "Personalized Volume",
    },
    SettingDef {
        key: "adaptive-audio-level",
        id: Id::AutoAncStrength,
        kind: Kind::Int { min: 0, max: 100 },
        label: "Adaptive Audio level (raw, 0-100)",
    },
    SettingDef {
        key: "one-bud-anc",
        id: Id::OneBudAncMode,
        kind: Kind::Bool,
        label: "Noise Cancellation with one AirPod",
    },
    SettingDef {
        key: "ear-detection",
        id: Id::EarDetectionConfig,
        kind: Kind::Bool,
        label: "Automatic ear detection",
    },
    SettingDef {
        key: "sleep-detection",
        id: Id::SleepDetectionConfig,
        kind: Kind::Bool,
        label: "Pause media when falling asleep",
    },
    SettingDef {
        key: "volume-swipe",
        id: Id::VolumeSwipeMode,
        kind: Kind::Bool,
        label: "Volume control (swipe)",
    },
    SettingDef {
        key: "volume-swipe-length",
        id: Id::VolumeSwipeInterval,
        kind: Kind::Choice(SWIPE_LENGTHS),
        label: "Volume swipe length",
    },
    SettingDef {
        key: "press-speed",
        id: Id::DoubleClickInterval,
        kind: Kind::Choice(SPEEDS),
        label: "Press speed",
    },
    SettingDef {
        key: "press-hold-duration",
        id: Id::ClickHoldInterval,
        kind: Kind::Choice(SPEEDS),
        label: "Press and hold duration",
    },
];

pub fn by_key(key: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.key == key)
}

pub fn by_id(id: Id) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.id == id)
}

impl SettingDef {
    /// The value the AirPods reported (trailing zero bytes already trimmed).
    pub fn decode(&self, raw: &[u8]) -> Option<SettingValue> {
        let first = *raw.first()?;
        match self.kind {
            Kind::Bool => match first {
                0x01 => Some(SettingValue::Bool(true)),
                0x02 => Some(SettingValue::Bool(false)),
                _ => None,
            },
            Kind::Choice(choices) => choices
                .iter()
                .find(|(v, _)| *v == first)
                .map(|(_, name)| SettingValue::Str(name.to_string())),
            Kind::Int { .. } => Some(SettingValue::Int(first as i32)),
        }
    }

    pub fn encode(&self, value: &SettingValue) -> Result<Vec<u8>, String> {
        match (&self.kind, value) {
            (Kind::Bool, SettingValue::Bool(b)) => Ok(vec![if *b { 0x01 } else { 0x02 }]),
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
