//! Daemon behavior settings, read once at startup from `~/.config/podsd/config.json`.
//! Missing keys take their defaults; a missing file means all defaults.

use crate::utils::get_config_path;
use log::{info, warn};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Pause media when an AirPod is taken out, resume when it goes back in.
    pub ear_pause: bool,
    /// Switch the AirPods' audio profile off while neither AirPod is worn, so sound
    /// falls back to the next output. WirePlumber saves that profile choice, so with
    /// the daemon stopped the AirPods would come back silent; off by default.
    pub disconnect_when_not_wearing: bool,
    /// When media starts playing here with an AirPod in, ask the AirPods to switch to
    /// this computer (takes them from a phone that is using them).
    pub takeover_on_play: bool,
    /// Lower the volume while Conversation Awareness says you're speaking.
    pub conversation_volume: bool,
    /// Double press = next track, triple press = previous track, handled here.
    pub stem_control: bool,
    /// Restart WirePlumber when the AirPods have no A2DP profile. Restarting it
    /// interrupts all audio, so it is opt-in.
    pub restart_wireplumber: bool,
    /// Watch the AirPods' BLE adverts: case lid open (CaseOpen), battery while
    /// connected elsewhere. Needs `Experimental = true` in /etc/bluetooth/main.conf;
    /// without it this does nothing.
    pub ble_monitor: bool,
    /// With `bleMonitor`: connect the AirPods by themselves when their lid
    /// opens: while they're free, or with Apple's Device ID also next to
    /// another device that has them. Off: a client offers Connect instead.
    pub auto_connect: bool,
    /// CaseOpen counts only adverts at least this strong (dBm): a case nearby,
    /// not one across the room.
    pub case_open_rssi: i16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ear_pause: true,
            disconnect_when_not_wearing: false,
            takeover_on_play: true,
            conversation_volume: true,
            stem_control: false,
            restart_wireplumber: false,
            ble_monitor: true,
            auto_connect: false,
            case_open_rssi: -70,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let path = get_config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(config) => {
                    info!("Loaded config from {}", path.display());
                    config
                }
                Err(e) => {
                    warn!("Ignoring {}: {}", path.display(), e);
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }
}
