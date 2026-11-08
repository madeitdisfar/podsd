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

use crate::bluetooth::aacp::{AACPManager, AudioSourceType, ConnectedDevice, ControlCommandIdentifiers};
use crate::hub::Hub;
use crate::media_controller::{
    MediaController, local_audio_active_sync, playing_players_sync, resume_players_sync,
};
use log::{debug, error, info, warn};
use std::collections::HashSet;
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
    /// The AirPods have reported ownership at least once
    reported: bool,
    /// Audio turned off here because another device has it
    off_for_other: bool,
    /// Until then, losing the audio is the AirPods' choice on reinsertion, not
    /// another device taking them: take them straight back
    reclaim_until: Option<Instant>,
    /// When this computer last took them over: audio source reports right after
    /// can still name the other device, which mustn't make it give them back
    took_at: Option<Instant>,
    /// Other devices sent this computer's media information and "new tipi"
    /// message since they (last) connected
    greeted: HashSet<String>,
    /// The last priority list sent: the other host, and whether this computer
    /// went first; when; and how many this session
    listed: Option<(String, bool)>,
    listed_at: Option<Instant>,
    lists_sent: u8,
    /// The other device last seen connected this session: the list names it
    /// even while it's briefly gone or still rejoining
    partner: Option<String>,
}

/// Device list flags (an entry's second byte; EarPort and kase read them so,
/// and LISTED matches what podsd saw: the laptop never had it): on the
/// AirPods' priority list, and the device they play from
const HOST_LISTED: u8 = 0x01;
const HOST_PLAYS: u8 = 0x02;
/// The same priority list again only after this; at most so many a session
const LIST_REPEAT: Duration = Duration::from_secs(2);
const LISTS_MAX: u8 = 8;


/// A connected-devices entry's first info byte: 0 disconnected, 1 still
/// connecting, 2 connected
const DEVICE_CONNECTED: u8 = 2;

/// Audio source reports this soon after taking over are ignored
const TAKE_SETTLE: Duration = Duration::from_secs(2);
/// After an AirPod goes back in, losing the audio this soon is the AirPods'
/// own choice (they hand it to the other device while nothing plays here)
const RECLAIM_WINDOW: Duration = Duration::from_secs(3);

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
        Switcher {
            hub,
            address,
            aacp,
            media,
            state: Arc::new(Mutex::new(State::default())),
        }
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

    /// The other devices that are fully connected: the only ones messages
    /// can be relayed to. Relaying to a device still connecting made the
    /// AirPods drop this computer ("Low Resources", 0.2 s later, every time).
    async fn reachable_others(&self) -> Vec<String> {
        let local = self.hub.local_mac().to_ascii_uppercase();
        let state = self.aacp.state.lock().await;
        state
            .connected_devices
            .iter()
            .filter(|d| d.info1 == DEVICE_CONNECTED && d.mac.to_ascii_uppercase() != local)
            .map(|d| d.mac.clone())
            .collect()
    }

    /// Asks the AirPods to keep this computer connected alongside the other
    /// device: their priority list, the device they play from first, as a Mac
    /// sends it when sharing them with an iPhone. Without it the laptop was
    /// never on the list (its entry never had HOST_LISTED), and it was the one
    /// they dropped. Sent when another device is connected and this computer
    /// isn't listed, or the order changed. Also when the list is forgotten:
    /// when the main role moves to the other AirPod (one going into the case),
    /// that AirPod doesn't know the list (the laptop's LISTED flag went away),
    /// and the iPhone rejoining a second later dropped the laptop. So the
    /// other device is remembered once seen connected, and the list goes out
    /// at once even while it's away or still rejoining.
    async fn keep_listed(&self, devices: &[ConnectedDevice]) {
        let local = self.hub.local_mac().to_ascii_uppercase();
        let Some(me) = devices.iter().find(|d| d.mac.to_ascii_uppercase() == local) else {
            return;
        };
        let connected_other = devices
            .iter()
            .find(|d| d.mac.to_ascii_uppercase() != local && d.info1 == DEVICE_CONNECTED)
            .map(|d| d.mac.clone());
        let other = {
            let mut st = self.state.lock().await;
            if connected_other.is_some() {
                st.partner = connected_other;
            }
            st.partner.clone()
        };
        let Some(other) = other else { return };
        let mine_first = me.info2 & HOST_PLAYS != 0;
        let key = (other.clone(), mine_first);
        {
            let mut st = self.state.lock().await;
            let same = st.listed.as_ref() == Some(&key);
            if (same && me.info2 & HOST_LISTED != 0)
                || (same && st.listed_at.is_some_and(|t| t.elapsed() < LIST_REPEAT))
                || st.lists_sent >= LISTS_MAX
            {
                return;
            }
            st.listed = Some(key);
            st.listed_at = Some(Instant::now());
            st.lists_sent += 1;
        }
        let local = self.hub.local_mac().to_string();
        let hosts = if mine_first { [local, other.clone()] } else { [other.clone(), local] };
        info!(
            "{}: asking the AirPods to keep this computer with {} ({} first; this computer's flags {:#04x})",
            self.address,
            other,
            if mine_first { "this computer" } else { "the other device" },
            me.info2
        );
        if let Err(e) = self.aacp.send_priority_list(&hosts).await {
            warn!("{}: sending the priority list failed: {}", self.address, e);
        }
    }

    /// Introduces this computer to other devices once they are connected
    /// (Android: on a new connected device, media information and "add tipi
    /// device"). One still connecting is introduced when a later list shows
    /// it connected; one that leaves is introduced again when it's back.
    pub async fn greet(&self, devices: &[ConnectedDevice]) {
        self.keep_listed(devices).await;
        let local = self.hub.local_mac().to_ascii_uppercase();
        let new: Vec<String> = {
            let mut st = self.state.lock().await;
            st.greeted.retain(|mac| {
                devices.iter().any(|d| &d.mac == mac && d.info1 == DEVICE_CONNECTED)
            });
            let new: Vec<String> = devices
                .iter()
                .filter(|d| d.info1 == DEVICE_CONNECTED && d.mac.to_ascii_uppercase() != local)
                .filter(|d| !st.greeted.contains(&d.mac))
                .map(|d| d.mac.clone())
                .collect();
            st.greeted.extend(new.iter().cloned());
            new
        };
        let local_mac = self.hub.local_mac().to_string();
        for mac in new {
            info!("New device on the AirPods: {}, sending media info", mac);
            if let Err(e) = self.aacp.send_media_information_new_device(&local_mac, &mac).await {
                error!("Failed to send media info to new device: {}", e);
            }
            if let Err(e) = self.aacp.send_add_tipi_device(&local_mac, &mac).await {
                error!("Failed to send add tipi device: {}", e);
            }
        }
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
            st.off_for_other = false;
            st.took_at = Some(Instant::now());
            // Owning from now: the AirPods confirm a moment later, and a device
            // list arriving in between must not count this computer as not
            // owning (that turned the audio off right after every takeover)
            st.owned = true;
            st.reported = true;
            r
        };
        let local = self.hub.local_mac().to_string();
        let others = self.reachable_others().await;
        info!(
            "{}: taking the AirPods over{} from {:?}",
            self.address,
            if manual { " by hand" } else { "" },
            others
        );
        self.media.set_elsewhere(false).await;
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

    /// An AirPod went in or out of an ear (any_in: at least one is in now).
    /// Ear detection pauses playback here when the last one comes out, so this
    /// computer stops streaming; when one goes back in, the AirPods then gave
    /// the audio to the other device (seen every time with an iPhone). An Apple
    /// device resumes where it paused, so podsd claims the audio again before
    /// resuming, and takes it back if the AirPods hand it away anyway.
    /// Whatever made this computer the owner while no AirPod was in an ear
    /// counts: owning at removal, or "Use here" with both in the case (they
    /// handed the audio to the iPhone 1.9 s after one went in, 2026-11-08).
    /// Another device taking them meanwhile ends the ownership, so then the
    /// AirPods stay with it, as on Apple devices.
    pub async fn on_ear(&self, was_in: bool, any_in: bool) {
        if was_in || !any_in {
            return;
        }
        let reclaim = {
            let mut st = self.state.lock().await;
            let r = st.owned;
            if r {
                st.reclaim_until = Some(Instant::now() + RECLAIM_WINDOW);
            }
            r
        };
        if !reclaim {
            return;
        }
        info!("{}: AirPod back in, claiming the audio again", self.address);
        let local = self.hub.local_mac().to_string();
        let _ = self
            .aacp
            .send_control_command(ControlCommandIdentifiers::OwnsConnection, &[0x01])
            .await;
        for other in self.reachable_others().await {
            let _ = self.aacp.send_media_information(&local, &other, true).await;
        }
    }

    /// The AirPods reported who owns the audio
    pub async fn on_ownership(&self, owns: bool) {
        let reclaiming = !owns
            && self
                .state
                .lock()
                .await
                .reclaim_until
                .is_some_and(|t| Instant::now() < t);
        if reclaiming {
            info!("{}: the AirPods gave the audio away on reinsertion, taking it back", self.address);
            self.state.lock().await.reclaim_until = None;
            self.take_over(true).await;
            return;
        }
        let lost = {
            let mut st = self.state.lock().await;
            let lost = st.owned && !owns;
            st.owned = owns;
            st.reported = true;
            if lost {
                st.lost_at = Some(Instant::now());
                st.paused_for_other = true;
                st.off_for_other = true;
            }
            lost
        };
        if owns {
            self.state.lock().await.off_for_other = false;
            self.media.set_elsewhere(false).await;
            // Given back by the AirPods themselves (the other device left or
            // stopped): the audio profile switched off when they left comes back
            self.media.restore_profile_if_off().await;
        }
        if lost {
            info!("{}: another device took the AirPods", self.address);
            self.media.set_elsewhere(true).await;
            self.media.pause_all_media().await;
            let _ = self.media.deactivate_a2dp_profile().await;
            let name = self.other_device().await;
            self.hub.emit_moved_to(&self.address, &name, false).await;
        }
        self.refresh_other().await;
    }

    /// Another device took the AirPods with a hijack (SetOwnershipToFalse)
    pub async fn on_taken(&self, sender: &str, reversed: bool) {
        {
            let mut st = self.state.lock().await;
            st.owned = false;
            st.reported = true;
            st.off_for_other = true;
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
        self.media.set_elsewhere(true).await;
        self.media.pause_all_media().await;
        let _ = self.media.deactivate_a2dp_profile().await;
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
            // The AirPods don't always send an ownership report then (right
            // after connecting they hadn't), so take this as one: otherwise
            // this computer kept the AirPods as its output and competed for
            // the audio, and the phone's playback paused.
            self.on_ownership(false).await;
            return;
        }
        self.refresh_other().await;
    }

    /// Before any ownership report (the AirPods send none at connect), takes
    /// ownership from the device list: bit 0x02 of an entry's second info
    /// byte marks the device that owns the audio (it matched every
    /// OwnsConnection report seen, for this computer and for an iPhone).
    async fn ownership_from_devices(&self) {
        if self.state.lock().await.reported {
            return;
        }
        let local = self.hub.local_mac().to_ascii_uppercase();
        let owner = {
            let st = self.aacp.state.lock().await;
            st.connected_devices
                .iter()
                .find(|d| d.info2 & 0x02 != 0)
                .map(|d| d.mac.to_ascii_uppercase() == local)
        };
        let Some(owns) = owner else { return };
        {
            let mut st = self.state.lock().await;
            if st.reported {
                return;
            }
            st.owned = owns;
            st.reported = true;
        }
        info!(
            "{}: the device list says {} owns the audio",
            self.address,
            if owns { "this computer" } else { "another device" }
        );
        self.hub.update(&self.address, |s| s.owns_audio = owns).await;
    }

    /// Updates OtherDevice; with no other device left, this computer is the
    /// only one again
    pub async fn refresh_other(&self) {
        self.ownership_from_devices().await;
        // Back after the AirPods dropped this computer for a device joining to
        // play (hub::Release::JoinedWhileUnlisted): if that device has the
        // audio, it took the AirPods, so report the move as on_ownership would
        let from_join = self.state.lock().await.reported && self.hub.take_back_from_join(&self.address).await;
        let other = self.other_device().await;
        if other.is_empty() {
            self.media.set_elsewhere(false).await;
        } else {
            // Another device there and the AirPods say this computer doesn't
            // own the audio (often at connect, where the ownership report comes
            // before the device list): no sound here, ear detection is theirs.
            // Only with another device there: alone, the AirPods may never
            // claim ownership for this computer at all.
            let turn_off = {
                let mut st = self.state.lock().await;
                let settling = st.took_at.is_some_and(|t| t.elapsed() < TAKE_SETTLE);
                let off = st.reported && !st.owned && !st.off_for_other && !settling;
                if off {
                    st.off_for_other = true;
                }
                off
            };
            if turn_off && from_join {
                {
                    let mut st = self.state.lock().await;
                    st.lost_at = Some(Instant::now());
                    st.paused_for_other = true;
                }
                info!("{}: {} took the AirPods as it joined", self.address, other);
                self.hub.emit_moved_to(&self.address, &other, false).await;
            }
            if turn_off {
                self.media.set_elsewhere(true).await;
                if !self.media.deactivate_a2dp_profile().await {
                    // At connect the audio card shows up a moment after the
                    // device list: try again while the audio is still elsewhere
                    let this = self.clone();
                    tokio::spawn(async move {
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let still = {
                                let st = this.state.lock().await;
                                st.off_for_other && !st.owned
                            };
                            if !still || this.media.deactivate_a2dp_profile().await {
                                return;
                            }
                        }
                    });
                }
            }
        }
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
            // Changes podsd made itself (ear detection) aren't the user's
            if changed && !self.media.recent_self_action(Duration::from_secs(2)).await {
                let local = self.hub.local_mac().to_string();
                for other in self.reachable_others().await {
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
