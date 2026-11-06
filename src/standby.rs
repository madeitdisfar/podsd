//! Audio standby: while this computer owns the AirPods' audio but plays
//! nothing, their A2DP stream is closed (the audio profile off), as a Mac
//! leaves it closed until sound plays. Linux opens the stream at connect and
//! keeps it open; with it open, another device joining the AirPods (an iPhone
//! reconnecting, or picked as its output) made them drop this computer
//! ("Low Resources"), every time. With it closed, the iPhone joins and this
//! computer stays.
//!
//! The AirPods stay this computer's output meanwhile: a stand-in output with
//! their name (a null sink) takes their place, and podsd brings the real one
//! back, moving the sound over, as soon as anything plays on it.
//!
//! These helpers run `pactl` (blocking: call them from spawn_blocking).

use log::{debug, warn};
use serde_json::Value;
use std::process::Command;

/// Stand-in sinks are named this, plus the AirPods' address
const PREFIX: &str = "podsd_standby_";

pub fn stand_in_name(mac: &str) -> String {
    format!("{PREFIX}{}", mac.replace(':', "_"))
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    if !out.status.success() {
        debug!("pactl {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pactl_json(what: &str) -> Vec<Value> {
    pactl(&["-f", "json", "list", what])
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

/// An output: its index, name, description, and volume (raw, per channel)
#[derive(Debug, Clone)]
pub struct Sink {
    pub index: u64,
    pub name: String,
    pub description: String,
    pub volume: Vec<u64>,
    pub muted: bool,
}

/// The audio right now: outputs, playback streams, and the default output
pub struct Snapshot {
    sinks: Vec<Value>,
    inputs: Vec<Value>,
    pub default: String,
}

impl Snapshot {
    pub fn take() -> Self {
        Snapshot {
            sinks: pactl_json("sinks"),
            inputs: pactl_json("sink-inputs"),
            default: pactl(&["get-default-sink"]).unwrap_or_default(),
        }
    }

    fn sink(v: &Value) -> Sink {
        Sink {
            index: v["index"].as_u64().unwrap_or(u64::MAX),
            name: v["name"].as_str().unwrap_or_default().to_string(),
            description: v["description"].as_str().unwrap_or_default().to_string(),
            volume: v["volume"]
                .as_object()
                .map(|channels| channels.values().filter_map(|c| c["value"].as_u64()).collect())
                .unwrap_or_default(),
            muted: v["mute"].as_bool().unwrap_or(false),
        }
    }

    /// The AirPods' high-quality (A2DP) output, if their profile is on
    pub fn airpods_sink(&self, mac: &str) -> Option<Sink> {
        self.sinks
            .iter()
            .find(|s| {
                let p = &s["properties"];
                p["api.bluez5.address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(mac))
                    && p["api.bluez5.profile"].as_str().is_some_and(|pr| pr.starts_with("a2dp"))
            })
            .map(Self::sink)
    }

    pub fn find(&self, name: &str) -> Option<Sink> {
        self.sinks.iter().find(|s| s["name"].as_str() == Some(name)).map(Self::sink)
    }

    /// Streams on this output: (index, playing)
    fn inputs_on(&self, sink: u64) -> Vec<(u64, bool)> {
        self.inputs
            .iter()
            .filter(|i| i["sink"].as_u64() == Some(sink))
            .filter_map(|i| Some((i["index"].as_u64()?, !i["corked"].as_bool().unwrap_or(false))))
            .collect()
    }

    /// Whether anything plays on this output (system sounds included: on a Mac
    /// they play through the AirPods too)
    pub fn playing_on(&self, sink: u64) -> bool {
        self.inputs_on(sink).iter().any(|(_, playing)| *playing)
    }
}

/// Adds the stand-in output, named as the AirPods' own. Returns its module.
pub fn add_stand_in(mac: &str, description: &str) -> Option<u32> {
    // Inside the quoted properties, quotes and backslashes would end them early
    let description: String = description.chars().filter(|c| !matches!(c, '"' | '\'' | '\\')).collect();
    let module = pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={}", stand_in_name(mac)),
        &format!(
            "sink_properties='device.description=\"{description}\" device.icon_name=audio-headphones-bluetooth podsd.standby=1'"
        ),
    ])?;
    module.parse().ok()
}

pub fn remove_stand_in(module: u32) {
    if pactl(&["unload-module", &module.to_string()]).is_none() {
        warn!("Couldn't remove the AirPods' stand-in output (module {})", module);
    }
}

/// Removes stand-ins left behind by an earlier run that didn't end cleanly
pub fn remove_stale_stand_ins() {
    // "index<TAB>name<TAB>argument" (the JSON listing has no index)
    let list = pactl(&["list", "short", "modules"]).unwrap_or_default();
    for line in list.lines() {
        let mut fields = line.split('\t');
        let (Some(index), Some(name), Some(argument)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if name == "module-null-sink"
            && argument.contains(&format!("sink_name={PREFIX}"))
            && let Ok(index) = index.parse::<u32>()
        {
            debug!("Removing a stale AirPods stand-in (module {})", index);
            remove_stand_in(index);
        }
    }
}

/// Gives an output another's volume and mute, so switching between the two
/// changes neither (one value for all channels when their counts differ)
pub fn copy_volume(from: &Sink, to: &str, to_channels: usize) {
    if from.volume.is_empty() {
        return;
    }
    let values: Vec<String> = if from.volume.len() == to_channels {
        from.volume.iter().map(u64::to_string).collect()
    } else {
        vec![(from.volume.iter().sum::<u64>() / from.volume.len() as u64).to_string()]
    };
    let mut args = vec!["set-sink-volume", to];
    args.extend(values.iter().map(String::as_str));
    let _ = pactl(&args);
    let _ = pactl(&["set-sink-mute", to, if from.muted { "1" } else { "0" }]);
}

pub fn set_default(sink: &str) {
    let _ = pactl(&["set-default-sink", sink]);
}

/// Moves every stream on one output to another
pub fn move_streams(snapshot: &Snapshot, from: u64, to: &str) {
    for (index, _) in snapshot.inputs_on(from) {
        let _ = pactl(&["move-sink-input", &index.to_string(), to]);
    }
}
