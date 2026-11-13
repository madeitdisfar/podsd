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
    /// Whether this computer streams to the AirPods (Switcher::streaming), as
    /// last told to the other devices
    told_streaming: bool,
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
    /// Until then, losing the audio is the AirPods' own choice, not another
    /// device taking them: take it straight back (once). Opened by a bud going
    /// in while none was worn (B7), and by the other device rejoining just
    /// after they let it go (B27)
    reclaim_until: Option<Instant>,
    /// When the AirPods last let the other device go while this computer had
    /// the audio
    other_left_at: Option<Instant>,
    /// Other devices sent this computer's media information and "new tipi"
    /// message since they (last) connected
    greeted: HashSet<String>,
    /// The last priority list sent: the other host, and whether this computer
    /// went first; when; and when each list went out this last minute
    listed: Option<(String, bool)>,
    listed_at: Option<Instant>,
    lists_sent: Vec<Instant>,
    /// The other device last seen connected this session: the list names it
    /// even while it's briefly gone or still rejoining
    partner: Option<String>,
}

/// Device list flags (an entry's second byte; EarPort and kase read them so,
/// and LISTED matches what podsd saw: the laptop never had it): on the
/// AirPods' priority list, and the device they play from
const HOST_LISTED: u8 = 0x01;
const HOST_PLAYS: u8 = 0x02;
/// The same priority list again only after this; at most so many a minute.
/// Each change of owner sends one (the playing host goes first), so a cap per
/// session ran out in busy ones (8 by 11:04 on 11-13; the main AirPod
/// changing after that left this computer unlisted for good)
const LIST_REPEAT: Duration = Duration::from_secs(2);
const LISTS_PER_MINUTE: usize = 6;


/// A connected-devices entry's first info byte: 0 disconnected, 1 still
/// connecting, 2 connected
const DEVICE_CONNECTED: u8 = 2;
const DEVICE_DISCONNECTED: u8 = 0;

/// Losing the audio this soon after a bud went in with none worn, or after
/// the other device rejoined, is the AirPods' own choice (handed over
/// 0.14-1.2 s after either)
const RECLAIM_WINDOW: Duration = Duration::from_secs(3);
/// The other device back this soon after the AirPods let it go rejoined by
/// itself (1-7 s seen, 10 times); later, it's the user picking the AirPods
/// on it (68 s, 11-13 20:04), and it keeps what it takes
const REJOIN_WINDOW: Duration = Duration::from_secs(15);

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

    /// The other devices on the AirPods, connected or connecting (connected
    /// devices report, minus this computer and entries marked disconnected)
    async fn others(&self) -> Vec<String> {
        let local = self.hub.local_mac().to_ascii_uppercase();
        let state = self.aacp.state.lock().await;
        state
            .connected_devices
            .iter()
            .filter(|d| d.info1 != DEVICE_DISCONNECTED && d.mac.to_ascii_uppercase() != local)
            .map(|d| d.mac.clone())
            .collect()
    }

    /// The other devices that are fully connected: the ones messages are
    /// relayed to. (Drops 0.2 s after greeting a device still connecting were
    /// first blamed on the greeting; the cause was this computer missing from
    /// the priority list. Waiting for a device to connect is kept: one still
    /// connecting may not get the message.)
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
            st.lists_sent.retain(|t| t.elapsed() < Duration::from_secs(60));
            if (same && me.info2 & HOST_LISTED != 0)
                || (same && st.listed_at.is_some_and(|t| t.elapsed() < LIST_REPEAT))
            {
                return;
            }
            if st.lists_sent.len() >= LISTS_PER_MINUTE {
                debug!("{}: {} priority lists this minute already, not sending another", self.address, LISTS_PER_MINUTE);
                return;
            }
            st.listed = Some(key);
            let now = Instant::now();
            st.listed_at = Some(now);
            st.lists_sent.push(now);
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
            let before = st.greeted.len();
            st.greeted.retain(|mac| {
                devices.iter().any(|d| &d.mac == mac && d.info1 == DEVICE_CONNECTED)
            });
            if st.greeted.len() < before && st.owned {
                st.other_left_at = Some(Instant::now());
            }
            let new: Vec<String> = devices
                .iter()
                .filter(|d| d.info1 == DEVICE_CONNECTED && d.mac.to_ascii_uppercase() != local)
                .filter(|d| !st.greeted.contains(&d.mac))
                .map(|d| d.mac.clone())
                .collect();
            st.greeted.extend(new.iter().cloned());
            // B27: as the main AirPod changes with a bud worn, the AirPods let
            // the other device go, and give it the audio 0.25-1.2 s after it
            // rejoins, whatever this computer told it (8 of 8 times)
            let rejoined = !new.is_empty() && st.other_left_at.take().is_some_and(|t| t.elapsed() < REJOIN_WINDOW);
            if rejoined && st.owned {
                info!("{}: the other device rejoined: taking the audio back if the AirPods hand it over", self.address);
                st.reclaim_until = Some(Instant::now() + RECLAIM_WINDOW);
            }
            new
        };
        let local_mac = self.hub.local_mac().to_string();
        // A device rejoining while this computer played heard "not streaming"
        // and took the audio 0.25-0.65 s later (4 of 4 times, 2026-11-11/12;
        // and 11-13 19:58:53, when the greeting went by the pause ear
        // detection had just made)
        let was_active = self.state.lock().await.was_active;
        let streaming = self.streaming(was_active).await;
        for mac in new {
            info!("New device on the AirPods: {}, sending media info (streaming here: {})", mac, streaming);
            if let Err(e) = self.aacp.send_media_information_new_device(&local_mac, &mac, streaming).await {
                error!("Failed to send media info to new device: {}", e);
            }
            if let Err(e) = self.aacp.send_add_tipi_device(&local_mac, &mac).await {
                error!("Failed to send add tipi device: {}", e);
            }
        }
    }

    /// Whether this computer streams to the AirPods, as the other devices are
    /// told: it owns their audio, and audio plays here or ear detection paused
    /// it to resume (as a Mac paused by ear detection: the bud going back in
    /// resumes it, so the other device mustn't take them meanwhile)
    async fn streaming(&self, active: bool) -> bool {
        let owned = self.state.lock().await.owned;
        owned && (active || self.media.paused_for_resume().await)
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
    /// When one goes in with none worn, the AirPods give the audio to the
    /// other device (B7, every time with an iPhone), so podsd takes it back if
    /// they do. Claiming it first didn't stop them (handed over 0.14 s after
    /// the claim, 11-13 20:11:58), so nothing is sent beforehand. Whatever
    /// made this computer the owner while no AirPod was in an ear counts:
    /// owning at removal, or "Use here" with both in the case (handed over
    /// 1.9 s after one went in, 2026-11-08). Another device taking them
    /// meanwhile ends the ownership, so then the AirPods stay with it.
    pub async fn on_ear(&self, was_in: bool, any_in: bool) {
        if was_in || !any_in {
            return;
        }
        let mut st = self.state.lock().await;
        if st.owned {
            info!("{}: AirPod in with none worn: taking the audio back if the AirPods hand it over", self.address);
            st.reclaim_until = Some(Instant::now() + RECLAIM_WINDOW);
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
            info!("{}: the AirPods gave the audio away by themselves, taking it back", self.address);
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
        // Only before any ownership report. After one, every real takeover
        // came with OwnsConnection 00 (25 of 25, 11-11 to 11-13), and the
        // iPhone's audio reports while this computer owned the audio came
        // only right after a take-back: giving up on one (11-12 15:30:09) is
        // what made the AirPods' stream fail and BlueZ end the link. Not
        // owning, there's nothing to give up either: the iPhone reports its
        // audio again and again while it plays (hundreds of times an hour).
        let reported = self.state.lock().await.reported;
        if kind != AudioSourceType::None && mac.to_ascii_uppercase() != local && !reported {
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
                let off = st.reported && !st.owned && !st.off_for_other;
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
                // At connect the audio card can still be missing: on_card
                // turns it off once it's there
                let _ = self.media.deactivate_a2dp_profile().await;
            }
        }
        self.hub.update(&self.address, |s| s.other_device = other).await;
    }

    /// The AirPods' audio card is there (at connect it comes a moment after
    /// the control channel): its profile as the audio's owner wants it. Off
    /// while another device has the audio; otherwise on, as WirePlumber
    /// remembers the "off" podsd set last time, and the AirPods connecting
    /// alone and worn then played nothing here.
    pub async fn on_card(&self) {
        let elsewhere = {
            let st = self.state.lock().await;
            st.off_for_other && !st.owned
        };
        if elsewhere {
            let _ = self.media.deactivate_a2dp_profile().await;
        } else {
            self.media.restore_profile_if_off().await;
        }
    }

    /// Follows local playback: tells the other devices when it starts and stops,
    /// and takes the AirPods over when it starts (automatic switching).
    pub async fn run(self, automatic: bool, apple_id: bool) {
        loop {
            tokio::time::sleep(POLL).await;
            let active = tokio::task::spawn_blocking(local_audio_active_sync).await.unwrap_or(false);
            let (started, blocked) = {
                let mut st = self.state.lock().await;
                let started = active && !st.was_active;
                st.was_active = active;
                let recently_lost = st.lost_at.is_some_and(|t| t.elapsed() < LOST_GRACE);
                (started, recently_lost)
            };
            // Right after podsd paused or resumed by itself, the players are
            // still on their way: no news to tell yet
            let own_doing = self.media.recent_self_action(Duration::from_secs(2)).await;
            if started && !own_doing {
                self.media.forget_paused().await;
            }
            // Only audio played to the AirPods counts: sound on the speakers
            // while another device owns them isn't this computer streaming
            let streaming = self.streaming(active).await;
            let changed = streaming != self.state.lock().await.told_streaming;
            if changed && !own_doing {
                self.state.lock().await.told_streaming = streaming;
                let local = self.hub.local_mac().to_string();
                for other in self.reachable_others().await {
                    if let Err(e) = self.aacp.send_media_information(&local, &other, streaming).await {
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
