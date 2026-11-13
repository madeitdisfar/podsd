use crate::bluetooth::aacp::EarDetectionStatus;
use dbus::arg::RefArg;
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::SinkInfo;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
use libpulse_binding::def::Retval;
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::operation::State as OperationState;
use libpulse_binding::proplist::Proplist;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
struct OwnedCardProfileInfo {
    name: Option<String>,
}

#[derive(Clone, Debug)]
struct OwnedCardInfo {
    index: u32,
    proplist: Proplist,
    profiles: Vec<OwnedCardProfileInfo>,
    active_profile: Option<String>,
}

#[derive(Clone, Debug)]
struct OwnedSinkInfo {
    name: Option<String>,
    proplist: Proplist,
    volume: ChannelVolumes,
}

struct MediaControllerState {
    connected_device_mac: String,
    local_mac: String,
    is_playing: bool,
    paused_by_app_services: Vec<String>,
    device_index: Option<u32>,
    cached_a2dp_profile: String,
    old_in_ear_data: Vec<bool>,
    user_played_the_media: bool,
    i_paused_the_media: bool,
    ear_detection_enabled: bool,
    disconnect_when_not_wearing: bool,
    /// Another device has the AirPods' audio: ear detection is its business
    elsewhere: bool,
    /// When podsd itself last paused or resumed playback (ear detection)
    self_action_at: Option<std::time::Instant>,
    conv_original_volume: Option<u32>,
    conv_conversation_started: bool,
    restart_wireplumber: bool,
}

impl MediaControllerState {
    fn new() -> Self {
        MediaControllerState {
            connected_device_mac: String::new(),
            local_mac: String::new(),
            is_playing: false,
            paused_by_app_services: Vec::new(),
            device_index: None,
            cached_a2dp_profile: String::new(),
            old_in_ear_data: vec![false, false],
            user_played_the_media: false,
            i_paused_the_media: false,
            ear_detection_enabled: true,
            disconnect_when_not_wearing: false,
            elsewhere: false,
            self_action_at: None,
            conv_original_volume: None,
            conv_conversation_started: false,
            restart_wireplumber: false,
        }
    }
}

#[derive(Clone)]
pub struct MediaController {
    state: Arc<Mutex<MediaControllerState>>,
}

impl MediaController {
    pub fn new(connected_mac: String, local_mac: String) -> Self {
        let mut state = MediaControllerState::new();
        state.connected_device_mac = connected_mac;
        state.local_mac = local_mac;
        MediaController {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Whether podsd paused or resumed playback itself within `within`: such
    /// changes aren't announced to the other device (LibrePods' Android app
    /// ignores playback changes just after its own actions), which otherwise
    /// heard "stopped streaming" when an AirPod came out and got the audio
    /// when it went back in
    pub async fn recent_self_action(&self, within: Duration) -> bool {
        self.state.lock().await.self_action_at.is_some_and(|t| t.elapsed() < within)
    }

    async fn mark_self_action(&self) {
        self.state.lock().await.self_action_at = Some(std::time::Instant::now());
    }

    /// Ear detection paused players here and will resume them when the buds
    /// are back in
    pub async fn paused_for_resume(&self) -> bool {
        !self.state.lock().await.paused_by_app_services.is_empty()
    }

    /// Playback started here other than by podsd: if it's one of the players
    /// ear detection paused, the user took it over, and the list is no longer
    /// for ear detection to resume (a bud going back in would start it after
    /// a pause by hand since). Other sound (a chat's ping) leaves it.
    pub async fn forget_paused_if_resumed(&self) {
        let paused = self.state.lock().await.paused_by_app_services.clone();
        if paused.is_empty() {
            return;
        }
        let playing = tokio::task::spawn_blocking(playing_players_sync).await.unwrap_or_default();
        if paused.iter().any(|p| playing.contains(p)) {
            self.forget_paused().await;
        }
    }

    /// What ear detection paused is no longer for it to resume (another
    /// device took the audio, or the user played it again)
    pub async fn forget_paused(&self) {
        let mut state = self.state.lock().await;
        if !state.paused_by_app_services.is_empty() {
            debug!("Forgetting the players ear detection paused: {:?}", state.paused_by_app_services);
            state.paused_by_app_services.clear();
        }
    }

    /// Another device has the AirPods' audio (or this computer has it again)
    pub async fn set_elsewhere(&self, elsewhere: bool) {
        self.state.lock().await.elsewhere = elsewhere;
    }

    pub async fn set_options(&self, disconnect_when_not_wearing: bool, restart_wireplumber: bool) {
        let mut state = self.state.lock().await;
        state.disconnect_when_not_wearing = disconnect_when_not_wearing;
        state.restart_wireplumber = restart_wireplumber;
    }

    /// Nothing to stop now that the switching loop lives in switching.rs; kept
    /// so a session's shutdown reads the same.
    pub async fn stop(&self) {}

    /// The MPRIS players that play audio on this computer: the player's process
    /// (or one it started, as browsers play from child processes) has an audio
    /// stream here. Leaves out remote controls such as Spotify Connect, which
    /// report Playing while a phone plays: pausing or resuming them pauses or
    /// resumes the phone, which then takes the AirPods back.
    fn local_players(conn: &Connection) -> Vec<String> {
        let streams = stream_pids_sync();
        let dbus = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(2));
        Self::list_mpris_services(conn)
            .into_iter()
            .filter(|service| {
                let pid: Option<u32> = dbus
                    .method_call("org.freedesktop.DBus", "GetConnectionUnixProcessID", (service.as_str(),))
                    .ok()
                    .map(|(pid,): (u32,)| pid);
                let local = pid.is_some_and(|pid| streams.iter().any(|s| descends_from(*s, pid)));
                if !local {
                    debug!("{} plays nothing here (a remote control?), leaving it alone", service);
                }
                local
            })
            .collect()
    }

    fn is_kdeconnect_service(service: &str) -> bool {
        service.starts_with("org.mpris.MediaPlayer2.kdeconnect.mpris_")
    }

    pub async fn handle_ear_detection(
        &self,
        old_statuses: Vec<EarDetectionStatus>,
        new_statuses: Vec<EarDetectionStatus>,
    ) {
        // While another device has the audio, it pauses and resumes its own
        // playback, as between Apple devices. Acting here too turned the
        // AirPods' audio back on (they became this computer's output again)
        // and could pause the phone through a remote player.
        {
            let mut state = self.state.lock().await;
            if state.elsewhere {
                debug!("Ear detection: another device has the audio, leaving it to it");
                state.old_in_ear_data = new_statuses.iter().map(|s| *s == EarDetectionStatus::InEar).collect();
                return;
            }
        }
        debug!(
            "Entering handle_ear_detection with old_statuses: {:?}, new_statuses: {:?}",
            old_statuses, new_statuses
        );

        let old_in_ear_data: Vec<bool> = old_statuses
            .iter()
            .map(|s| *s == EarDetectionStatus::InEar)
            .collect();
        let new_in_ear_data: Vec<bool> = new_statuses
            .iter()
            .map(|s| *s == EarDetectionStatus::InEar)
            .collect();

        let in_ear = new_in_ear_data.iter().all(|&b| b);
        let old_all_out = old_in_ear_data.iter().all(|&b| !b);
        let new_has_at_least_one_in = new_in_ear_data.iter().any(|&b| b);
        let new_all_out = new_in_ear_data.iter().all(|&b| !b);

        debug!(
            "Computed states: in_ear={}, old_all_out={}, new_has_at_least_one_in={}, new_all_out={}",
            in_ear, old_all_out, new_has_at_least_one_in, new_all_out
        );

        // The first report after connecting is where the AirPods already are, not
        // a change: acting on it paused media and switched the audio profile off
        // just because the buds were in their case.
        if old_statuses.is_empty() {
            debug!("First ear detection report, recording it as the baseline");
            self.state.lock().await.old_in_ear_data = new_in_ear_data;
            return;
        }

        {
            let state = self.state.lock().await;
            if !state.ear_detection_enabled {
                debug!("Ear detection disabled, skipping");
                return;
            }
        }

        if new_has_at_least_one_in && old_all_out {
            debug!("Condition met: buds inserted, activating A2DP and checking play state");
            self.activate_a2dp_profile().await;
            {
                let mut state = self.state.lock().await;
                if state.is_playing {
                    state.user_played_the_media = true;
                    debug!("Set user_played_the_media to true as media was playing");
                }
            }
        } else if new_all_out {
            debug!("Condition met: buds removed, pausing media");
            self.pause().await;
            {
                let state = self.state.lock().await;
                if state.disconnect_when_not_wearing {
                    debug!("Disconnect when not wearing enabled, deactivating A2DP");
                    drop(state);
                    let _ = self.deactivate_a2dp_profile().await;
                }
            }
        }

        let reset_user_played = (old_in_ear_data.iter().any(|&b| !b)
            && new_in_ear_data.iter().all(|&b| b))
            || (new_in_ear_data.iter().any(|&b| !b) && old_in_ear_data.iter().all(|&b| b));
        if reset_user_played {
            debug!("Transition detected, resetting user_played_the_media");
            let mut state = self.state.lock().await;
            state.user_played_the_media = false;
        }

        info!(
            "Ear Detection - old_in_ear_data: {:?}, new_in_ear_data: {:?}",
            old_in_ear_data, new_in_ear_data
        );

        let mut old_sorted = old_in_ear_data.clone();
        old_sorted.sort();
        let mut new_sorted = new_in_ear_data.clone();
        new_sorted.sort();
        if new_sorted != old_sorted {
            debug!("Ear data changed, checking resume/pause logic");
            if in_ear {
                debug!("Resuming media as buds are in ear");
                self.resume().await;
                self.state.lock().await.i_paused_the_media = false;
            } else if !old_all_out {
                debug!("Pausing media as buds are not fully in ear");
                self.pause().await;
                self.state.lock().await.i_paused_the_media = true;
            } else {
                debug!("Playing media");
                self.resume().await;
                self.state.lock().await.i_paused_the_media = false;
            }
        }

        {
            let mut state = self.state.lock().await;
            state.old_in_ear_data = new_in_ear_data;
            debug!("Updated old_in_ear_data to {:?}", state.old_in_ear_data);
        }
    }

    pub async fn activate_a2dp_profile(&self) {
        debug!("Entering activate_a2dp_profile");
        let state = self.state.lock().await;

        if state.connected_device_mac.is_empty() {
            warn!("Connected device MAC is empty, cannot activate A2DP profile");
            return;
        }

        let device_index = state.device_index;
        let mac = state.connected_device_mac.clone();
        drop(state);

        let mut current_device_index = device_index;

        if current_device_index.is_none() {
            warn!("Device index not found, trying to get it.");
            current_device_index = self.get_audio_device_index(&mac).await;
            if let Some(idx) = current_device_index {
                self.state.lock().await.device_index = Some(idx);
            } else {
                warn!("Could not get device index. Cannot activate A2DP profile.");
                return;
            }
        }

        if !self.is_a2dp_profile_available().await {
            if !self.state.lock().await.restart_wireplumber {
                warn!("A2DP profile not available (restartWireplumber is off)");
                return;
            }
            warn!("A2DP profile not available, attempting to restart WirePlumber");
            if self.restart_wire_plumber().await {
                // Not holding the state: is_a2dp_profile_available takes it
                let index = self.get_audio_device_index(&mac).await;
                self.state.lock().await.device_index = index;
                debug!("Updated device_index after WirePlumber restart: {:?}", index);
                if !self.is_a2dp_profile_available().await {
                    error!("A2DP profile still not available after WirePlumber restart");
                    return;
                }
            } else {
                error!("Could not restart WirePlumber, A2DP profile unavailable");
                return;
            }
        }

        let preferred_profile = self.get_preferred_a2dp_profile().await;
        if preferred_profile.is_empty() {
            error!("No suitable A2DP profile found");
            return;
        }

        let device_index = self.state.lock().await.device_index;
        if let Some(idx) = device_index {
            info!("Activating A2DP profile for AirPods: {}", preferred_profile);
            let profile_name = preferred_profile.clone();
            let success =
                tokio::task::spawn_blocking(move || set_card_profile_sync(idx, &profile_name))
                    .await
                    .unwrap_or(false);

            if success {
                info!("Successfully activated A2DP profile: {}", preferred_profile);
            } else {
                warn!("Failed to activate A2DP profile: {}", preferred_profile);
            }
        } else {
            error!("Device index not available for activating profile.");
        }
    }

    async fn pause(&self) {
        debug!("Pausing playback");
        self.mark_self_action().await;

        let paused_services = tokio::task::spawn_blocking(|| {
            let conn = match Connection::new_session() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            let mut paused_services = Vec::new();

            for service in Self::local_players(&conn) {
                debug!("Checking playback status for service: {}", service);
                let proxy = conn.with_proxy(
                    &service,
                    "/org/mpris/MediaPlayer2",
                    Duration::from_secs(5),
                );

                if let Ok(playback_status) =
                    proxy.get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
                    && playback_status == "Playing"
                {
                    if proxy
                        .method_call::<(), _, &str, &str>(
                            "org.mpris.MediaPlayer2.Player",
                            "Pause",
                            (),
                        )
                        .is_ok()
                    {
                        info!("Paused playback for: {}", service);
                        paused_services.push(service);
                    } else {
                        error!("Failed to pause {}", service);
                    }
                }
            }

            paused_services
        })
            .await
            .unwrap();

        if !paused_services.is_empty() {
            info!("Paused {} media player(s) via DBus", paused_services.len());
            let mut state = self.state.lock().await;
            state.paused_by_app_services = paused_services;
            state.is_playing = false;
        } else {
            info!("No playing media players found to pause");
        }
    }

    pub async fn pause_all_media(&self) {
        debug!("Pausing all media (without tracking for resume)");
        // Another device took the audio: what ear detection paused before
        // isn't to be resumed by a bud going back in
        self.forget_paused().await;

        let paused_count = tokio::task::spawn_blocking(|| {
            let conn = match Connection::new_session() {
                Ok(c) => c,
                Err(_) => return 0,
            };
            let mut paused_count = 0;

            for service in Self::local_players(&conn) {
                let proxy =
                    conn.with_proxy(&service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
                if let Ok(playback_status) =
                    proxy.get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
                    && playback_status == "Playing"
                {
                    if proxy
                        .method_call::<(), _, &str, &str>(
                            "org.mpris.MediaPlayer2.Player",
                            "Pause",
                            (),
                        )
                        .is_ok()
                    {
                        info!("Paused playback for: {}", service);
                        paused_count += 1;
                    } else {
                        error!("Failed to pause {}", service);
                    }
                }
            }
            paused_count
        })
            .await
            .unwrap();

        if paused_count > 0 {
            info!("Paused {} media player(s) due to ownership loss", paused_count);
            self.state.lock().await.is_playing = false;
        } else {
            debug!("No playing media players found to pause");
        }
    }

    async fn resume(&self) {
        debug!("Resuming playback");
        self.mark_self_action().await;
        // Resumed once: failed or not, they're not podsd's to resume later
        let services = std::mem::take(&mut self.state.lock().await.paused_by_app_services);

        if services.is_empty() {
            debug!("No services to resume");
            return;
        }

        let resumed_count = tokio::task::spawn_blocking(move || {
            let conn = match Connection::new_session() {
                Ok(c) => c,
                Err(_) => return 0,
            };
            let mut resumed_count = 0;
            for service in services {
                let proxy =
                    conn.with_proxy(&service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
                if proxy
                    .method_call::<(), _, &str, &str>("org.mpris.MediaPlayer2.Player", "Play", ())
                    .is_ok()
                {
                    info!("Resumed playback for: {}", service);
                    resumed_count += 1;
                } else {
                    warn!("Failed to resume {}", service);
                }
            }
            resumed_count
        })
            .await
            .unwrap();

        if resumed_count > 0 {
            info!("Resumed {} media player(s) via DBus", resumed_count);
        } else {
            error!("Failed to resume any media players via DBus");
        }
    }

    pub async fn next_track(&self) {
        info!("Skipping to next track");
        self.mpris_command("Next").await;
    }

    pub async fn previous_track(&self) {
        info!("Going to previous track");
        self.mpris_command("Previous").await;
    }

    async fn is_a2dp_profile_available(&self) -> bool {
        let index = match self.state.lock().await.device_index {
            Some(i) => i,
            None => {
                debug!("Device index is None, returning false");
                return false;
            }
        };

        tokio::task::spawn_blocking(move || {
            let available = get_card_info_list_sync()
                .iter()
                .find(|c| c.index == index)
                .map(|card| {
                    card.profiles
                        .iter()
                        .any(|p| p.name.as_ref().is_some_and(|n| n.starts_with("a2dp-sink")))
                })
                .unwrap_or(false);
            debug!("A2DP profile available: {}", available);
            available
        })
            .await
            .unwrap_or(false)
    }

    async fn get_preferred_a2dp_profile(&self) -> String {
        let state = self.state.lock().await;
        let device_index = state.device_index;
        let cached_profile = state.cached_a2dp_profile.clone();
        drop(state);

        let index = match device_index {
            Some(i) => i,
            None => return String::new(),
        };

        if !cached_profile.is_empty() && self.is_profile_available(index, &cached_profile).await {
            debug!("Using cached A2DP profile: {}", cached_profile);
            return cached_profile;
        }

        // "a2dp-sink" is the codec WirePlumber picks first (AAC for AirPods); the
        // SBC variants are fallbacks.
        for profile in ["a2dp-sink", "a2dp-sink-sbc_xq", "a2dp-sink-sbc"] {
            if self.is_profile_available(index, profile).await {
                info!("Selected best available A2DP profile: {}", profile);
                self.state.lock().await.cached_a2dp_profile = profile.to_string();
                return profile.to_string();
            }
        }
        debug!("No suitable profile found");
        String::new()
    }

    async fn is_profile_available(&self, card_index: u32, profile: &str) -> bool {
        let profile_name = profile.to_string();
        tokio::task::spawn_blocking(move || {
            let available = get_card_info_list_sync()
                .iter()
                .find(|c| c.index == card_index)
                .map(|card| {
                    card.profiles
                        .iter()
                        .any(|p| p.name.as_ref() == Some(&profile_name))
                })
                .unwrap_or(false);
            debug!("Profile {} available: {}", profile_name, available);
            available
        })
            .await
            .unwrap_or(false)
    }

    async fn restart_wire_plumber(&self) -> bool {
        info!("Restarting WirePlumber to rediscover A2DP profiles");
        let result = Command::new("systemctl")
            .args(["--user", "restart", "wireplumber"])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                info!("WirePlumber restarted successfully");
                tokio::time::sleep(Duration::from_secs(2)).await;
                true
            }
            _ => {
                error!("Failed to restart WirePlumber. Do you use wireplumber?");
                false
            }
        }
    }

    async fn get_audio_device_index(&self, mac: &str) -> Option<u32> {
        let index = card_index(mac).await;
        match index {
            Some(i) => info!("Found audio device index for MAC {}: {}", mac, i),
            None => error!("No matching Bluetooth card found for MAC address: {}", mac),
        }
        index
    }

    /// Waits for the AirPods' audio card: right after connecting, it comes a
    /// moment after the control channel (or never, with audio off in BlueZ).
    /// Whether it's there.
    pub async fn wait_for_card(&self, limit: Duration) -> bool {
        let mac = self.state.lock().await.connected_device_mac.clone();
        let start = std::time::Instant::now();
        loop {
            if let Some(index) = card_index(&mac).await {
                self.state.lock().await.device_index = Some(index);
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Switches the AirPods' audio profile off. Returns whether it did (false
    /// while their audio card isn't there yet, as right after connecting).
    pub async fn deactivate_a2dp_profile(&self) -> bool {
        debug!("Entering deactivate_a2dp_profile");
        let mut state = self.state.lock().await;

        if state.device_index.is_none() {
            state.device_index = self
                .get_audio_device_index(&state.connected_device_mac)
                .await;
        }

        if state.connected_device_mac.is_empty() || state.device_index.is_none() {
            warn!("Connected device MAC or index is empty, cannot deactivate A2DP profile");
            return false;
        }
        let device_index = state.device_index.unwrap();
        drop(state);

        // Remember the codec in use, so turning the AirPods back on restores it.
        if let Some(active) = tokio::task::spawn_blocking(move || active_profile_sync(device_index))
            .await
            .ok()
            .flatten()
            && active.starts_with("a2dp-sink")
        {
            self.state.lock().await.cached_a2dp_profile = active;
        }

        info!("Deactivating A2DP profile for AirPods by setting to off");

        let success = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                set_card_profile_sync(device_index, "off")
            }))
                .unwrap_or_else(|e| {
                    warn!("Panic in set_card_profile_sync: {:?}", e);
                    false
                })
        })
            .await
            .unwrap_or(false);

        if success {
            info!("Successfully deactivated A2DP profile");
        } else {
            warn!("Failed to deactivate A2DP profile");
            // The card may have gone (and come back under a new index)
            self.state.lock().await.device_index = None;
        }
        success
    }

    /// Turns the AirPods' audio back on if their profile is "off" (which WirePlumber
    /// remembers across reconnects). Run when a session starts and when it ends, so
    /// the AirPods aren't left silent without the daemon.
    pub async fn restore_profile_if_off(&self) {
        let mac = self.state.lock().await.connected_device_mac.clone();
        let Some(index) = self.get_audio_device_index(&mac).await else {
            return;
        };
        let active = tokio::task::spawn_blocking(move || active_profile_sync(index))
            .await
            .ok()
            .flatten();
        if active.as_deref() == Some("off") {
            info!("AirPods audio profile is off, turning it back on");
            self.state.lock().await.device_index = Some(index);
            self.activate_a2dp_profile().await;
        }
    }

    pub async fn handle_conversational_awareness(&self, status: u8) {
        debug!("Entering handle_conversational_awareness with status: {}", status);

        let mac = self.state.lock().await.connected_device_mac.clone();
        if mac.is_empty() {
            debug!("No connected device MAC, skipping conversational awareness");
            return;
        }

        let sink = match get_sink_name_by_mac(&mac).await {
            Some(s) => s,
            None => {
                warn!(
                    "Could not find sink for MAC {}, skipping conversational awareness",
                    mac
                );
                return;
            }
        };

        let current_volume_opt = tokio::task::spawn_blocking({
            let sink = sink.clone();
            move || get_sink_volume_percent_by_name_sync(&sink)
        })
            .await
            .unwrap_or(None);

        match status {
            1 => {
                let original = current_volume_opt.unwrap_or(0);
                debug!("Conversation start (1). Current volume: {}", original);
                {
                    let mut state = self.state.lock().await;
                    if !state.conv_conversation_started {
                        state.conv_original_volume = Some(original);
                        state.conv_conversation_started = true;
                    } else {
                        debug!(
                            "Conversation already started; not overwriting conv_original_volume"
                        );
                    }
                }
                if original > 25 {
                    let sink_clone = sink.clone();
                    tokio::task::spawn_blocking(move || transition_sink_volume(&sink_clone, 25))
                        .await
                        .unwrap_or(false);
                    info!(
                        "Conversation start: lowered volume to 25% (original {})",
                        original
                    );
                } else {
                    debug!("Original volume {} <= 25, not reducing to 25", original);
                }
            }
            2 => {
                let original = self.state.lock().await.conv_original_volume;
                if let Some(orig) = original {
                    debug!("Conversation reduce (2). Original: {}", orig);
                    if orig > 15 {
                        let sink_clone = sink.clone();
                        tokio::task::spawn_blocking(move || {
                            transition_sink_volume(&sink_clone, 15)
                        })
                            .await
                            .unwrap_or(false);
                        info!(
                            "Conversation reduce: lowered volume to 15% (original {})",
                            orig
                        );
                    } else {
                        debug!("Original {} <= 15, not reducing to 15", orig);
                    }
                } else {
                    debug!("No original volume known for status 2, skipping");
                }
            }
            3 => {
                let (conv_started, conv_original) = {
                    let state = self.state.lock().await;
                    (state.conv_conversation_started, state.conv_original_volume)
                };
                if !conv_started {
                    debug!("Received status 3 but conversation was not started; ignoring increase");
                    return;
                }
                if let Some(orig) = conv_original {
                    let target = orig.min(25);
                    let sink_clone = sink.clone();
                    tokio::task::spawn_blocking(move || transition_sink_volume(&sink_clone, target))
                        .await
                        .unwrap_or(false);
                    info!(
                        "Conversation partial increase (3): set volume to {} (original {})",
                        target, orig
                    );
                } else if let Some(orig_from_current) = current_volume_opt {
                    let target = orig_from_current.min(25);
                    let sink_clone = sink.clone();
                    tokio::task::spawn_blocking(move || transition_sink_volume(&sink_clone, target))
                        .await
                        .unwrap_or(false);
                    info!(
                        "Conversation partial increase (3) with fallback current: set volume to {} (measured {})",
                        target, orig_from_current
                    );
                } else {
                    debug!("No original volume known for status 3, skipping");
                }
            }
            4 | 6 | 7 => {
                debug!("Conversation end ({}), restoring volume if needed", status);
                self.restore_volume_if_needed(&sink).await;
            }
            _ => {
                debug!("Conversation status ({}), ignoring", status);
            }
        }
    }

    async fn mpris_command(&self, command: &'static str) {
        tokio::task::spawn_blocking(move || {
            let conn = match Connection::new_session() {
                Ok(c) => c,
                Err(_) => return,
            };

            let services = Self::list_mpris_services(&conn);
            let mut playing = None;
            let mut fallback = None;

            for service in &services {
                let proxy =
                    conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
                if let Ok(status) =
                    proxy.get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
                {
                    if status == "Playing" && playing.is_none() {
                        playing = Some(service);
                    }
                }
                if fallback.is_none() {
                    fallback = Some(service);
                }
            }

            if let Some(service) = playing.or(fallback) {
                let proxy =
                    conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
                if proxy
                    .method_call::<(), _, &str, &str>(
                        "org.mpris.MediaPlayer2.Player",
                        command,
                        (),
                    )
                    .is_ok()
                {
                    info!("Sent {} to: {}", command, service);
                } else {
                    debug!("Failed to send {} to: {}", command, service);
                }
            }
        })
            .await
            .unwrap();
    }

    async fn restore_volume_if_needed(&self, sink: &str) {
        let maybe_original = {
            let mut state = self.state.lock().await;
            if state.conv_conversation_started {
                let orig = state.conv_original_volume;
                state.conv_original_volume = None;
                state.conv_conversation_started = false;
                orig
            } else {
                return;
            }
        };

        if let Some(orig) = maybe_original {
            let sink = sink.to_string();
            tokio::task::spawn_blocking(move || transition_sink_volume(&sink, orig))
                .await
                .unwrap_or(false);
        }
    }

    fn list_mpris_services(conn: &Connection) -> Vec<String> {
        let proxy = conn.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            Duration::from_secs(5),
        );

        let (names,): (Vec<String>,) =
            match proxy.method_call("org.freedesktop.DBus", "ListNames", ()) {
                Ok(n) => n,
                Err(_) => return vec![],
            };

        names
            .into_iter()
            .filter(|s| {
                s.starts_with("org.mpris.MediaPlayer2.") && !Self::is_kdeconnect_service(s)
            })
            .collect()
    }
}

// --- PulseAudio helpers ---

fn pulse_connect() -> Option<(Mainloop, Context)> {
    let mut mainloop = Mainloop::new()?;
    let mut context = Context::new(&mainloop, "LibrePods")?;
    context.connect(None, ContextFlagSet::NOAUTOSPAWN, None).ok()?;
    loop {
        mainloop.iterate(false);
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed
            | libpulse_binding::context::State::Terminated => return None,
            _ => {}
        }
    }
    Some((mainloop, context))
}

fn get_card_info_list_sync() -> Vec<OwnedCardInfo> {
    let (mut mainloop, context) = match pulse_connect() {
        Some(c) => c,
        None => return vec![],
    };

    let introspector = context.introspect();
    let cards: Rc<RefCell<Vec<OwnedCardInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let op = introspector.get_card_info_list({
        let cards = cards.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                let profiles = item
                    .profiles
                    .iter()
                    .map(|p| OwnedCardProfileInfo {
                        name: p.name.as_ref().map(|n| n.to_string()),
                    })
                    .collect();
                cards.borrow_mut().push(OwnedCardInfo {
                    index: item.index,
                    proplist: item.proplist.clone(),
                    profiles,
                    active_profile: item
                        .active_profile
                        .as_ref()
                        .and_then(|p| p.name.as_ref().map(|n| n.to_string())),
                });
            }
        }
    });

    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }
    mainloop.quit(Retval(0));
    Rc::try_unwrap(cards).unwrap().into_inner()
}

/// Whether an app here is playing audio now: an unpaused playback stream
/// (PulseAudio sink input), not counting system sounds, notifications or
/// accessibility. A player that only reports "Playing" over MPRIS (Spotify
/// Connect playing on a phone) has no stream, so it doesn't count. LibrePods'
/// Android app likewise looks at the audio actually playing (music or movie).
pub fn local_audio_active_sync() -> bool {
    let (mut mainloop, context) = match pulse_connect() {
        Some(c) => c,
        None => return false,
    };
    let active = Rc::new(RefCell::new(false));
    let op = context.introspect().get_sink_input_info_list({
        let active = active.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                let role = item.proplist.get_str("media.role").unwrap_or_default();
                let ignored = ["event", "notification", "a11y", "accessibility", "test"]
                    .iter()
                    .any(|r| role.eq_ignore_ascii_case(r));
                if !item.corked && !ignored {
                    *active.borrow_mut() = true;
                }
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }
    mainloop.quit(Retval(0));
    let result = *active.borrow();
    result
}

/// MPRIS players reporting Playing now (to resume them after a switch)
pub fn playing_players_sync() -> Vec<String> {
    let Ok(conn) = Connection::new_session() else {
        return vec![];
    };
    MediaController::local_players(&conn)
        .into_iter()
        .filter(|service| {
            conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(2))
                .get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
                .map(|s| s == "Playing")
                .unwrap_or(false)
        })
        .collect()
}

/// Plays those of these players that aren't playing now. The AirPods pause the
/// audio while they switch, so playback is resumed after taking them over, as
/// LibrePods' Android app does.
pub fn resume_players_sync(players: &[String]) {
    let Ok(conn) = Connection::new_session() else {
        return;
    };
    for service in players {
        let proxy = conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(2));
        let playing = proxy
            .get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
            .map(|s| s == "Playing")
            .unwrap_or(true);
        if !playing {
            let _ = proxy.method_call::<(), _, &str, &str>("org.mpris.MediaPlayer2.Player", "Play", ());
            info!("Resumed {} after taking the AirPods over", service);
        }
    }
}

/// The processes behind the audio streams here: the stream's
/// application.process.id, else its client's. Apps on PipeWire's own API
/// (Spotify) put the process only on the client, not on the stream.
fn stream_pids_sync() -> Vec<u32> {
    let (mut mainloop, context) = match pulse_connect() {
        Some(c) => c,
        None => return vec![],
    };
    fn pid_of(proplist: &Proplist, keys: &[&str]) -> Option<u32> {
        keys.iter()
            .find_map(|k| proplist.get_str(k).and_then(|p| p.parse::<u32>().ok()))
    }
    let pids = Rc::new(RefCell::new(Vec::new()));
    // Streams without a process: their clients
    let clients = Rc::new(RefCell::new(Vec::new()));
    let op = context.introspect().get_sink_input_info_list({
        let pids = pids.clone();
        let clients = clients.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                match pid_of(&item.proplist, &["application.process.id"]) {
                    Some(pid) => pids.borrow_mut().push(pid),
                    None => clients.borrow_mut().extend(item.client),
                }
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }
    if !clients.borrow().is_empty() {
        let op = context.introspect().get_client_info_list({
            let pids = pids.clone();
            let clients = clients.clone();
            move |result| {
                if let ListResult::Item(item) = result
                    && clients.borrow().contains(&item.index)
                    && let Some(pid) =
                        pid_of(&item.proplist, &["application.process.id", "pipewire.sec.pid"])
                {
                    pids.borrow_mut().push(pid);
                }
            }
        });
        while op.get_state() == OperationState::Running {
            mainloop.iterate(false);
        }
    }
    mainloop.quit(Retval(0));
    let list = pids.borrow().clone();
    list
}

/// Whether `pid` is `ancestor` or one of its descendants
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        // /proc/<pid>/stat: "pid (comm) state ppid …"; comm may hold spaces
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            return false;
        };
        let Some(ppid) = after.split_whitespace().nth(1).and_then(|p| p.parse().ok()) else {
            return false;
        };
        pid = ppid;
    }
    false
}

/// The audio card of the Bluetooth device with this address
async fn card_index(mac: &str) -> Option<u32> {
    if mac.is_empty() {
        return None;
    }
    let mac = mac.to_string();
    tokio::task::spawn_blocking(move || {
        get_card_info_list_sync()
            .into_iter()
            .find(|card| card.proplist.get_str("device.string").is_some_and(|d| d.contains(&mac)))
            .map(|card| card.index)
    })
    .await
    .unwrap_or(None)
}

fn active_profile_sync(card_index: u32) -> Option<String> {
    get_card_info_list_sync()
        .into_iter()
        .find(|c| c.index == card_index)
        .and_then(|c| c.active_profile)
}

fn get_sink_volume_percent_by_name_sync(sink_name: &str) -> Option<u32> {
    let (mut mainloop, context) = pulse_connect()?;
    let introspector = context.introspect();
    let sink_info: Rc<RefCell<Option<OwnedSinkInfo>>> = Rc::new(RefCell::new(None));
    let op = introspector.get_sink_info_by_name(sink_name, {
        let sink_info = sink_info.clone();
        move |result: ListResult<&SinkInfo>| {
            if let ListResult::Item(item) = result {
                *sink_info.borrow_mut() = Some(OwnedSinkInfo {
                    name: item.name.as_ref().map(|s| s.to_string()),
                    proplist: item.proplist.clone(),
                    volume: item.volume,
                });
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }
    mainloop.quit(Retval(0));

    let borrowed = sink_info.borrow();
    let info = borrowed.as_ref()?;
    let channels = info.volume.len();
    if channels == 0 {
        return None;
    }
    let total: f64 = info.volume.get().iter().map(|v| v.0 as f64).sum();
    let percent = ((total / channels as f64 / Volume::NORMAL.0 as f64) * 100.0).round() as u32;
    Some(percent)
}

fn set_card_profile_sync(card_index: u32, profile_name: &str) -> bool {
    let (mut mainloop, context) = match pulse_connect() {
        Some(c) => c,
        None => return false,
    };
    let mut introspector = context.introspect();
    // The server's answer: a profile the card doesn't have, or a card gone
    // meanwhile, fails here
    let done = Rc::new(RefCell::new(false));
    let op = introspector.set_card_profile_by_index(card_index, profile_name, {
        let done = done.clone();
        Some(Box::new(move |success| *done.borrow_mut() = success))
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }
    mainloop.quit(Retval(0));
    let success = *done.borrow();
    success
}

pub fn transition_sink_volume(sink_name: &str, target_volume: u32) -> bool {
    let (mut mainloop, context) = match pulse_connect() {
        Some(c) => c,
        None => return false,
    };
    let mut introspector = context.introspect();
    let sink_info: Rc<RefCell<Option<OwnedSinkInfo>>> = Rc::new(RefCell::new(None));
    let op = introspector.get_sink_info_by_name(sink_name, {
        let sink_info = sink_info.clone();
        move |result: ListResult<&SinkInfo>| {
            if let ListResult::Item(item) = result {
                *sink_info.borrow_mut() = Some(OwnedSinkInfo {
                    name: item.name.as_ref().map(|s| s.to_string()),
                    proplist: item.proplist.clone(),
                    volume: item.volume,
                });
            }
        }
    });
    while op.get_state() == OperationState::Running {
        mainloop.iterate(false);
    }

    if let Some(info) = sink_info.borrow().as_ref() {
        let channels = info.volume.len();
        let mut new_volumes = ChannelVolumes::default();
        let raw =
            (((target_volume as f64) / 100.0) * Volume::NORMAL.0.as_f64().unwrap()).round() as u32;
        new_volumes.set(channels, Volume(raw));

        let op = introspector.set_sink_volume_by_name(sink_name, &new_volumes, None);
        while op.get_state() == OperationState::Running {
            mainloop.iterate(false);
        }
        mainloop.quit(Retval(0));
        true
    } else {
        error!("Sink not found: {}", sink_name);
        false
    }
}

async fn get_sink_name_by_mac(mac: &str) -> Option<String> {
    if mac.is_empty() {
        return None;
    }
    let mac_clone = mac.to_string();

    tokio::task::spawn_blocking(move || {
        let (mut mainloop, context) = pulse_connect()?;
        let introspector = context.introspect();
        let sink_list: Rc<RefCell<Vec<OwnedSinkInfo>>> = Rc::new(RefCell::new(Vec::new()));
        let op = introspector.get_sink_info_list({
            let sink_list = sink_list.clone();
            move |result: ListResult<&SinkInfo>| {
                if let ListResult::Item(item) = result {
                    sink_list.borrow_mut().push(OwnedSinkInfo {
                        name: item.name.as_ref().map(|s| s.to_string()),
                        proplist: item.proplist.clone(),
                        volume: item.volume,
                    });
                }
            }
        });
        while op.get_state() == OperationState::Running {
            mainloop.iterate(false);
        }
        mainloop.quit(Retval(0));

        for sink in sink_list.borrow().iter() {
            if let Some(device_string) = sink.proplist.get_str("device.string")
                && device_string.to_uppercase().contains(&mac_clone.to_uppercase())
                && let Some(name) = &sink.name
            {
                info!("Found sink name for MAC {}: {}", mac_clone, name);
                return Some(name.to_string());
            }
            if let Some(bluez_path) = sink.proplist.get_str("bluez.path") {
                let mac_from_path = bluez_path
                    .split('/')
                    .next_back()
                    .unwrap_or("")
                    .replace("dev_", "")
                    .replace('_', ":");
                if mac_from_path.eq_ignore_ascii_case(&mac_clone)
                    && let Some(name) = &sink.name
                {
                    info!("Found sink name for MAC {}: {}", mac_clone, name);
                    return Some(name.to_string());
                }
            }
        }
        error!("No matching sink found for MAC address: {}", mac_clone);
        None
    })
        .await
        .unwrap_or(None)
}
/// Pauses the players playing here (with an audio stream on this computer)
/// and says how many. Needs no session: used as the AirPods go.
pub fn pause_local_players_sync() -> usize {
    let Ok(conn) = Connection::new_session() else {
        return 0;
    };
    let mut paused = 0;
    for service in MediaController::local_players(&conn) {
        let proxy = conn.with_proxy(&service, "/org/mpris/MediaPlayer2", Duration::from_secs(2));
        let playing = proxy
            .get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
            .is_ok_and(|s| s == "Playing");
        if playing
            && proxy
                .method_call::<(), _, &str, &str>("org.mpris.MediaPlayer2.Player", "Pause", ())
                .is_ok()
        {
            info!("Paused {}", service);
            paused += 1;
        }
    }
    paused
}
