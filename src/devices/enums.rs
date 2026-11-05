use crate::devices::airpods::AirPodsInformation;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeviceType {
    AirPods,
    /// Kept so a devices file written by LibrePods (which also handles Nothing
    /// earbuds) still loads; podsd ignores these entries.
    Nothing,
}

impl Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceType::AirPods => write!(f, "AirPods"),
            DeviceType::Nothing => write!(f, "Nothing"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum DeviceInformation {
    AirPods(AirPodsInformation),
    Nothing(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceData {
    pub name: String,
    pub type_: DeviceType,
    pub information: Option<DeviceInformation>,
}
