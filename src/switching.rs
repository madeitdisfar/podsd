//! Moving the AirPods between this computer and another device (an iPhone),
//! as Apple devices do and as LibrePods' Android app does it (AirPodsService
//! takeOver / onOwnershipChangeReceived / onOwnershipToFalseRequest and
//! MediaController's playback callback). The AirPods stay connected to both;
//! one device at a time owns the audio (control command OwnsConnection), and
//! the devices tell each other through the AirPods (smart routing messages).
//!
//! Taking over: when audio starts playing here (a real stream, not just a
//! player saying Playing), automatically if `takeoverOnPlay` and this
//! computer has Apple's Device ID (as Android, which doesn't try otherwise),
//! or by hand (MoveHere). Losing them: pause, switch the audio profile off,
//! tell clients (MovedTo), and don't take them back by itself for 3 s; after
//! the other device's user tapped "Move back", not until asked by hand.

use crate::bluetooth::aacp::{AACPManager, AudioSourceType, ControlCommandIdentifiers};
use crate::hub::Hub;
use crate::media_controller::{
    MediaController, local_audio_active_sync, playing_players_sync, resume_players_sync,
};
use log::{debug, info, warn};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// No automatic takeover this long after losing the AirPods (Android: 3 s)
const LOST_GRACE: Duration = Duration::from_secs(3);
/// How often local playback is checked
const POLL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct State {
    /// Audio was playing here at the last check
    was_active: bool,
    /// When this computer last lost the AirPods
    lost_at: Option<Instant>,
    /// Paused here because another device took them (Android: pausedForOtherDevice)
    paused_for_other: bool,
    /// The other device's user tapped "Move back": only a manual MoveHere takes
    /// them again (Android: disconnectedBecauseReversed)
    reversed: bool,
    /// This computer owned the audio at the last report
    owned: bool,
    /// When this computer last took them over: audio source reports right after
    /// can still name the other device, which mustn't make it give them back
    took_at: Option<Instant>,
}

/// Audio source reports this soon after taking over are ignored
const TAKE_SETTLE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Switcher {
    hub: Hub,
    address: String,
    aacp: AACPManager,
    media: MediaController,
    state: Arc<Mutex<State>>,
}

impl Switcher {
    pub fn new(hub: Hub, address: String, aacp: AACPManager, media: MediaController) -> Self {
        Switcher { hub, address, aacp, media, state: Arc::new(Mutex::new(State::default())) }
    }

    /// The other devices on the AirPods (connected devices report, minus this
    /// computer)
    async fn others(&self) -> Vec<String> {
        let local = self.hub.local_mac().to_ascii_uppercase();
        let state = self.aacp.state.lock().await;
        state
            .connected_devices
            .iter()
            .map(|d| d.mac.clone())
            .filter(|m| m.to_ascii_uppercase() != local)
            .collect()
    }

    /// The kind of another device ("iPhone", …), for clients
    pub async fn device_name(&self, mac: &str) -> String {
        self.aacp
            .state
            .lock()
            .await
            .device_types
            .get(mac)
            .cloned()
            .unwrap_or_else(|| "another device".to_string())
    }

    /// The other device the AirPods are with, by name ("" while this computer
    /// owns them or no other device is connected)
    pub async fn other_device(&self) -> String {
        match self.others().await.first() {
            Some(mac) => self.device_name(mac).await,
            None => String::new(),
        }
    }

    /// Takes the AirPods over (Android's takeOver): ownership, this computer's
    /// media information, the other devices' "moved" UI and a hijack request
    /// (or, after the other device moved them back, the reversed hijack), then
    /// the audio profile, then playback resumed twice as the AirPods pause it.
    pub async fn take_over(&self, manual: bool) {
        let reversed = {
            let mut st = self.state.lock().await;
            if st.reversed && !manual {
                debug!("{}: not taking over: the other device moved them back", self.address);
                return;
            }
            let r = st.reversed;
            st.reversed = false;
            st.paused_for_other = false;
            st.took_at = Some(Instant::now());
            r
        };
        let local = self.hub.local_mac().to_string();
        let others = self.others().await;
        info!(
            "{}: taking the AirPods over{} from {:?}",
            self.address,
            if manual { " by hand" } else { "" },
            others
        );
        let players = tokio::task::spawn_blocking(playing_players_sync).await.unwrap_or_default();
        let _ = self
            .aacp
            .send_control_command(ControlCommandIdentifiers::OwnsConnection, &[0x01])
            .await;
        for other in &others {
            let _ = self.aacp.send_media_information(&local, other, true).await;
            if reversed {
                let _ = self.aacp.send_hijack_reversed(other).await;
            } else {
                let _ = self.aacp.send_smart_routing_show_ui(other).await;
                let _ = self.aacp.send_hijack_request(other).await;
            }
        }
        self.media.activate_a2dp_profile().await;
        if !players.is_empty() {
            let resume = players.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let p = resume.clone();
                let _ = tokio::task::spawn_blocking(move || resume_players_sync(&p)).await;
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let _ = tokio::task::spawn_blocking(move || resume_players_sync(&resume)).await;
            });
        }
    }

    /// The AirPods reported who owns the audio
    pub async fn on_ownership(&self, owns: bool) {
        let lost = {
            let mut st = self.state.lock().await;
            let lost = st.owned && !owns;
            st.owned = owns;
            if lost {
                st.lost_at = Some(Instant::now());
                st.paused_for_other = true;
            }
            lost
        };
        if lost {
            info!("{}: another device took the AirPods", self.address);
            self.media.pause_all_media().await;
            self.media.deactivate_a2dp_profile().await;
            let name = self.other_device().await;
            self.hub.emit_moved_to(&self.address, &name, false).await;
        } else if !owns && !self.others().await.is_empty() {
            // Connected while another device owns them: no sound here (only
            // with another device there: alone, the AirPods may not claim
            // ownership for this computer at all)
            self.media.deactivate_a2dp_profile().await;
        }
        self.refresh_other().await;
    }

    /// Another device took the AirPods with a hijack (SetOwnershipToFalse)
    pub async fn on_taken(&self, sender: &str, reversed: bool) {
        {
            let mut st = self.state.lock().await;
            st.owned = false;
            st.lost_at = Some(Instant::now());
            st.paused_for_other = true;
            if reversed {
                st.reversed = true;
            }
        }
        let _ = self
            .aacp
            .send_control_command(ControlCommandIdentifiers::OwnsConnection, &[0x00])
            .await;
        self.media.pause_all_media().await;
        self.media.deactivate_a2dp_profile().await;
        let name = self.device_name(sender).await;
        self.hub.emit_moved_to(&self.address, &name, reversed).await;
        self.refresh_other().await;
    }

    /// Another device asks to show its "moved" UI
    pub async fn on_show_nearby(&self, sender: &str) {
        let name = self.device_name(sender).await;
        if !self.state.lock().await.owned {
            self.hub.emit_moved_to(&self.address, &name, false).await;
        }
    }

    /// The AirPods reported which device is playing: another one playing means
    /// this computer gives up the audio (Android: "audio source is another
    /// device, better to give up aacp control")
    pub async fn on_audio_source(&self, mac: &str, kind: AudioSourceType) {
        let local = self.hub.local_mac().to_ascii_uppercase();
        let settling = self.state.lock().await.took_at.is_some_and(|t| t.elapsed() < TAKE_SETTLE);
        if kind != AudioSourceType::None && mac.to_ascii_uppercase() != local && !settling {
            debug!("{}: {} plays now, giving up the audio", self.address, mac);
            let _ = self
                .aacp
                .send_control_command(ControlCommandIdentifiers::OwnsConnection, &[0x00])
                .await;
        }
        self.refresh_other().await;
    }

    async fn refresh_other(&self) {
        let other = self.other_device().await;
        self.hub.update(&self.address, |s| s.other_device = other).await;
    }

    /// Follows local playback: tells the other devices when it starts and stops,
    /// and takes the AirPods over when it starts (automatic switching).
    pub async fn run(self, automatic: bool, apple_id: bool) {
        loop {
            tokio::time::sleep(POLL).await;
            let active = tokio::task::spawn_blocking(local_audio_active_sync).await.unwrap_or(false);
            let (started, changed, blocked) = {
                let mut st = self.state.lock().await;
                let started = active && !st.was_active;
                let changed = active != st.was_active;
                st.was_active = active;
                let recently_lost = st.lost_at.is_some_and(|t| t.elapsed() < LOST_GRACE);
                (started, changed, recently_lost)
            };
            if changed {
                let local = self.hub.local_mac().to_string();
                for other in self.others().await {
                    if let Err(e) = self.aacp.send_media_information(&local, &other, active).await {
                        warn!("Telling {} about playback here failed: {}", other, e);
                    }
                }
            }
            if !started || !automatic || !apple_id {
                continue;
            }
            if blocked {
                debug!("{}: playback started, but the AirPods just left: not taking over", self.address);
                continue;
            }
            let in_ear = self.hub.state(&self.address).await.is_some_and(|s| s.in_ear());
            let owns = self.state.lock().await.owned;
            let other_playing = {
                let st = self.aacp.state.lock().await;
                let local = self.hub.local_mac().to_ascii_uppercase();
                st.audio_source.as_ref().is_some_and(|a| {
                    a.r#type != AudioSourceType::None && a.mac.to_ascii_uppercase() != local
                })
            };
            if !in_ear {
                debug!("{}: playback started with no AirPod in an ear: not taking over", self.address);
                continue;
            }
            if owns && !other_playing {
                debug!("{}: playback started, already own the AirPods", self.address);
                continue;
            }
            self.take_over(false).await;
        }
    }
}
