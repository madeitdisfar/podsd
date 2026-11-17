//! One session with a pair of AirPods: opens the AACP control channel, then turns
//! its events into state (for D-Bus) and media actions (pause on removal, etc.).

use crate::bluetooth::aacp::{
    AACPEvent, AACPManager, AirPodsLEKeys, BatteryComponent, BatteryStatus,
    ControlCommandIdentifiers, ControlCommandStatus, EarDetectionStatus, ProximityKeyType, StemPressBudType,
    StemPressType,
};
use crate::hub::{Hub, Session};
use crate::switching::Switcher;
use crate::media_controller::MediaController;
use crate::settings;
use crate::settings::LISTENING_MODES;
use crate::state::Battery;
use bluer::Address;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, sleep};

const CONNECT_ATTEMPTS: u32 = 3;

/// How long the listening mode in the AirPods' answer to the handshake is
/// held back, waiting for the one they report once the feature flags apply
/// (0.2-0.5 s after it, C-55)
const ANSWER_MODE_HOLD: std::time::Duration = std::time::Duration::from_secs(1);

/// Holds back the listening mode in the AirPods' answer to the handshake. It's
/// read before the feature flags apply, so Adaptive shows in it as 01 (Off),
/// and the right value follows once they do (sent again as the answer comes).
/// Published as it was, the mode went from Off to Adaptive at every connect,
/// which shells took for a switch. The answer's value counts only if nothing
/// follows within ANSWER_MODE_HOLD.
#[derive(Default)]
struct AnswerMode {
    /// The information came: the answer's listening mode is next
    expecting: bool,
    /// The answer's listening mode, and until when it's held
    held: Option<(ControlCommandStatus, std::time::Instant)>,
}

impl AnswerMode {
    /// Whether to handle the event now (false: held back)
    fn admit(&mut self, event: &AACPEvent, now: std::time::Instant) -> bool {
        match event {
            // The answer opens with the information, then the listening mode
            AACPEvent::Information(_) => self.expecting = true,
            AACPEvent::ControlCommand(status) if status.identifier == ControlCommandIdentifiers::ListeningMode => {
                if std::mem::take(&mut self.expecting) {
                    self.held = Some((status.clone(), now + ANSWER_MODE_HOLD));
                    return false;
                }
                // A newer report: the held one is stale
                self.held = None;
            }
            // The answer went on without a listening mode
            AACPEvent::ControlCommand(_) => self.expecting = false,
            _ => {}
        }
        true
    }

    fn deadline(&self) -> Option<std::time::Instant> {
        self.held.as_ref().map(|(_, until)| *until)
    }

    /// Nothing followed in time: the answer's value stands
    fn release(&mut self) -> Option<AACPEvent> {
        self.held.take().map(|(status, _)| AACPEvent::ControlCommand(status))
    }
}

pub async fn start(hub: Hub, address: String) -> Option<Session> {
    let mac: Address = match address.parse() {
        Ok(a) => a,
        Err(e) => {
            error!("Bad address {}: {}", address, e);
            return None;
        }
    };
    info!("Opening the control channel to {}", address);

    let mut aacp = AACPManager::new();
    // Before the handshake: the AirPods answer it with their settings at once.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    aacp.set_event_channel(event_tx).await;

    let mut connected = false;
    for attempt in 1..=CONNECT_ATTEMPTS {
        if aacp.connect(mac).await {
            connected = true;
            break;
        }
        if attempt < CONNECT_ATTEMPTS {
            sleep(Duration::from_millis(1500)).await;
        }
    }
    if !connected {
        warn!("Could not open the control channel to {}", address);
        aacp.shutdown().await;
        return None;
    }

    if let Err(e) = aacp.send_handshake().await {
        error!("Handshake failed: {}", e);
    }
    sleep(Duration::from_millis(300)).await;
    if let Err(e) = aacp.send_set_feature_flags_packet().await {
        error!("Failed to set feature flags: {}", e);
    }
    sleep(Duration::from_millis(300)).await;
    if let Err(e) = aacp.send_notification_request().await {
        error!("Failed to request notifications: {}", e);
    }
    if let Err(e) = aacp.send_some_packet().await {
        error!("Failed to send packet 0x29: {}", e);
    }
    if let Err(e) = aacp
        .send_proximity_keys_request(vec![ProximityKeyType::Irk, ProximityKeyType::EncKey])
        .await
    {
        error!("Failed to request proximity keys: {}", e);
    }

    let config = hub.config().clone();
    if config.stem_control {
        // StemConfig bitmask: single=0x01, double=0x02, triple=0x04, long=0x08.
        // Double and triple only, so single press still plays/pauses on its own.
        info!("Enabling stem press reports for double and triple press");
        if let Err(e) = aacp
            .send_control_command(ControlCommandIdentifiers::StemConfig, &[0x06])
            .await
        {
            error!("Failed to enable stem press reports: {}", e);
        }
    }

    // Settings the AirPods never report: what was last set here, else the default
    let remembered = crate::utils::load_remembered().remove(&address).unwrap_or_default();
    hub.update(&address, |s| {
        s.ready = true;
        s.reconnecting = false;
        for (key, default) in settings::UNREPORTED {
            let value = remembered
                .get(*key)
                .cloned()
                .or(default.map(settings::SettingValue::Bool));
            if let Some(value) = value {
                s.settings.entry(key.to_string()).or_insert(value);
            }
        }
    })
    .await;

    let media = MediaController::new(address.clone(), hub.local_mac().to_string());
    media
        .set_options(config.disconnect_when_not_wearing, config.restart_wireplumber)
        .await;
    {
        let lowering = crate::conversation::Lowering {
            cut_db: config.conversation_cut.clamp(0.0, 60.0),
            ceiling: config.conversation_ceiling.min(100),
        };
        let (hub, address) = (hub.clone(), address.clone());
        let notice: crate::media_controller::VolumeNotice = std::sync::Arc::new(move |ms| {
            let (hub, address) = (hub.clone(), address.clone());
            Box::pin(async move { hub.emit_conversation_volume(&address, ms).await })
        });
        media.set_conversation(lowering, notice).await;
    }

    let mut tasks = Vec::new();

    // Control commands from the playback listener (ownership changes).
    let (command_tx, mut command_rx) =
        tokio::sync::mpsc::unbounded_channel::<(ControlCommandIdentifiers, Vec<u8>)>();
    {
        let aacp = aacp.clone();
        tasks.push(tokio::spawn(async move {
            while let Some((id, value)) = command_rx.recv().await {
                if let Err(e) = aacp.send_control_command(id, &value).await {
                    error!("Failed to send control command: {}", e);
                }
            }
        }));
    }

    // If the handshake got lost, asking once more brings the settings at once.
    // After a switch from another device (a phone), though, the AirPods hold
    // their settings back for about 30 s whatever is sent (retries at 2, 5,
    // 10 and 20 s were all ignored, B6); clients show the listening mode
    // as unknown until it arrives. The notification request can be lost on its
    // own too: when the AirPods are busy with another device as this one
    // connects, they answer the handshake seconds late and drop the request
    // sent after it, so the settings arrive but no battery, ear detection,
    // ownership or connected-devices news ever does. Ask again until both come.
    {
        let aacp = aacp.clone();
        let address = address.clone();
        tasks.push(tokio::spawn(async move {
            for wait in [3, 7, 20] {
                sleep(Duration::from_secs(wait)).await;
                let settings = aacp.has_listening_mode().await;
                let notifications = aacp.has_notifications().await;
                if settings && notifications {
                    return;
                }
                let missing = match (settings, notifications) {
                    (false, false) => "settings or notifications",
                    (false, true) => "settings",
                    _ => "notifications",
                };
                info!("{}: no {} from the AirPods yet, asking again", address, missing);
                if let Err(e) = aacp.request_state().await {
                    warn!("Asking {} for its state failed: {}", address, e);
                }
            }
        }));
    }

    // Sometimes the AirPods take the channel and then never say anything on it,
    // whatever is asked, even through three requests over 30 s (B23). A new
    // channel on the same link answers at once.
    {
        let aacp = aacp.clone();
        let hub = hub.clone();
        let address = address.clone();
        tasks.push(tokio::spawn(async move {
            sleep(Duration::from_secs(7)).await;
            if !aacp.has_heard().await {
                info!("{}: the AirPods haven't said anything on the control channel; opening a new one", address);
                // Reopening stops this session, and with it this task
                tokio::spawn(async move { hub.reopen_channel(&address).await });
            }
        }));
    }

    // Moving the AirPods between this computer and another device. Its loop
    // always runs (it tells the other device when playback here starts and
    // stops); it takes them over by itself only with takeoverOnPlay and Apple's
    // Device ID.
    let switcher = Switcher::new(hub.clone(), address.clone(), aacp.clone(), media.clone());
    tasks.push(tokio::spawn(switcher.clone().run(config.takeover_on_play, hub.apple_id())));

    // The audio profile, once the AirPods' audio card is there
    {
        let media = media.clone();
        let switcher = switcher.clone();
        let address = address.clone();
        tasks.push(tokio::spawn(async move {
            if media.wait_for_card(Duration::from_secs(30)).await {
                switcher.on_card().await;
                // A conversation's lowered volume the last link went with
                media.restore_left_over(Duration::from_secs(10)).await;
            } else {
                info!("{}: no audio card for the AirPods after 30 s", address);
            }
        }));
    }

    {
        let hub = hub.clone();
        let aacp = aacp.clone();
        let media = media.clone();
        let address = address.clone();
        let switcher = switcher.clone();
        tasks.push(tokio::spawn(async move {
            let mut answer_mode = AnswerMode::default();
            loop {
                let deadline = answer_mode.deadline();
                let event = tokio::select! {
                    event = event_rx.recv() => match event {
                        Some(event) => event,
                        None => break,
                    },
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                        deadline.unwrap_or_else(std::time::Instant::now),
                    )), if deadline.is_some() => {
                        if let Some(event) = answer_mode.release() {
                            debug!("{}: no listening mode after the feature flags; the handshake's stands", address);
                            handle_event(&hub, &aacp, &media, &switcher, &address, event).await;
                        }
                        continue;
                    }
                };
                if let AACPEvent::Disconnected = event {
                    // Stopping the session aborts this task, so hand it off.
                    let hub = hub.clone();
                    let address = address.clone();
                    tokio::spawn(async move { hub.on_channel_closed(&address).await });
                    break;
                }
                if !answer_mode.admit(&event, std::time::Instant::now()) {
                    continue;
                }
                handle_event(&hub, &aacp, &media, &switcher, &address, event).await;
            }
        }));
    }

    drop(command_tx);
    Some(Session { aacp, media, switcher: Some(switcher), tasks })
}

fn ear_name(status: &EarDetectionStatus) -> &'static str {
    match status {
        EarDetectionStatus::InEar => "in-ear",
        EarDetectionStatus::OutOfEar => "out-of-ear",
        EarDetectionStatus::InCase => "in-case",
        EarDetectionStatus::Disconnected => "disconnected",
    }
}

async fn handle_event(
    hub: &Hub,
    aacp: &AACPManager,
    media: &MediaController,
    switcher: &Switcher,
    address: &str,
    event: AACPEvent,
) {
    let config = hub.config();
    match event {
        AACPEvent::BatteryInfo(batteries) => {
            // With no bud in it, the buds can't read the case, but the case's own
            // advert may have just now (le::CaseAdvert): that reading stays live
            let case_reported = batteries
                .iter()
                .any(|b| b.component == BatteryComponent::Case && b.status != BatteryStatus::Disconnected);
            if case_reported {
                hub.forget_case_frame(address).await;
            }
            let case_frame_live = hub.case_frame_live(address).await;
            hub.update(address, |s| {
                for b in &batteries {
                    let absent = b.status == BatteryStatus::Disconnected;
                    let battery = Battery::reading(b.level as i32, b.status == BatteryStatus::Charging);
                    match b.component {
                        BatteryComponent::Left => s.battery_left = if absent { Battery::default() } else { battery },
                        BatteryComponent::Right => s.battery_right = if absent { Battery::default() } else { battery },
                        BatteryComponent::Headphone => {
                            s.battery_headphone = if absent { Battery::default() } else { battery }
                        }
                        // No bud in the case: it can't report, keep its last reading
                        BatteryComponent::Case => {
                            if !absent {
                                s.battery_case = battery;
                            } else if !case_frame_live {
                                s.battery_case = s.battery_case.kept();
                            }
                        }
                    }
                }
                s.battery_source = "aacp".to_string();
            })
            .await;
            if let Some(case) = batteries
                .iter()
                .find(|b| b.component == BatteryComponent::Case && b.status != BatteryStatus::Disconnected)
            {
                crate::utils::save_case_battery(address, case.level as i32, crate::state::now());
            }
        }
        AACPEvent::ControlCommand(status) => {
            let owns = (status.identifier == ControlCommandIdentifiers::OwnsConnection)
                .then(|| status.value.first().copied().unwrap_or(0) != 0);
            let def = settings::by_id(status.identifier);
            hub.update(address, |s| {
                if let Some(def) = def
                    && let Some(value) = def.decode(&status.value)
                {
                    s.settings.insert(def.key.to_string(), value);
                }
                if status.identifier == ControlCommandIdentifiers::ListeningMode {
                    s.listening_mode = status
                        .value
                        .first()
                        .and_then(|v| LISTENING_MODES.iter().find(|(raw, _)| raw == v))
                        .map(|(_, name)| name.to_string())
                        .unwrap_or_default();
                }
                if let Some(owns) = owns {
                    s.owns_audio = owns;
                }
            })
            .await;
            if let Some(owns) = owns {
                switcher.on_ownership(owns).await;
            }
            // No levels come while it's off, so none ends the conversation
            if status.identifier == ControlCommandIdentifiers::ConversationDetectConfig
                && status.value.first() == Some(&0x02)
            {
                media.end_conversation("Conversation Awareness turned off").await;
            }
        }
        AACPEvent::EarDetection(old, new) => {
            hub.update(address, |s| {
                s.ear_primary = new.first().map(ear_name).unwrap_or_default().to_string();
                s.ear_secondary = new.get(1).map(ear_name).unwrap_or_default().to_string();
            })
            .await;
            let in_ear = |v: &Vec<EarDetectionStatus>| v.contains(&EarDetectionStatus::InEar);
            let (was_in, any_in) = (in_ear(&old), in_ear(&new));
            if !any_in {
                media.end_conversation("No AirPod in an ear").await;
            }
            // The AirPods' own Automatic Ear Detection setting turns this off,
            // as on iOS (pause / resume stop there too). The AirPods never report
            // it, so this is the value set through podsd (or on by default).
            let enabled = hub
                .state(address)
                .await
                .is_some_and(|s| s.settings.get("ear-detection") != Some(&settings::SettingValue::Bool(false)));
            if config.ear_pause && enabled {
                // The first report after connecting is where the buds already
                // are, not a bud going in: claiming then sent OwnsConnection at
                // every connect (C-45)
                let first = old.is_empty();
                media.handle_ear_detection(old, new).await;
                // After the pause / resume above, so the claim comes with playback
                if !first {
                    switcher.on_ear(was_in, any_in).await;
                }
            }
        }
        AACPEvent::ConversationalAwareness(status) => {
            if config.conversation_volume {
                media.handle_conversational_awareness(status).await;
            }
        }
        AACPEvent::Information(info) => {
            // The information opens the AirPods' answer to the handshake. On
            // a link that just came up it comes 0.8-1.7 s late, and the
            // feature flags sent on the timer before it are lost: the AirPods
            // then report Adaptive to this computer as 01 (Off) while they're
            // in it. Sent again now they're listening (as Refresh does), first
            // thing, as the answer's own mode waits for them (AnswerMode).
            if let Err(e) = aacp.send_set_feature_flags_packet().await {
                warn!("{}: sending the feature flags again failed: {}", address, e);
            }
            hub.update(address, |s| {
                if !info.name.is_empty() {
                    s.name = info.name.clone();
                }
                s.model = info.model_number.clone();
                s.firmware = info.version1.clone();
            })
            .await;
            hub.sync_alias(address, &info.name, false).await;
        }
        AACPEvent::ConnectedDevices(_, new_devices) => {
            // For a drop by a device joining before this computer was listed
            // (hub::Release::JoinedWhileUnlisted)
            let local = hub.local_mac().to_ascii_uppercase();
            let listed_here = new_devices
                .iter()
                .find(|d| d.mac.to_ascii_uppercase() == local)
                .map(|d| d.info2 & 0x01 != 0);
            let others = || new_devices.iter().filter(|d| d.mac.to_ascii_uppercase() != local);
            let other_joining = others().any(|d| d.info1 == 1);
            let other_connected = others().any(|d| d.info1 == 2);
            hub.note_device_list(address, listed_here, other_joining, other_connected).await;
            switcher.refresh_other().await;
            switcher.greet(&new_devices).await;
        }
        AACPEvent::OwnershipToFalseRequest { sender, reversed } => {
            switcher.on_taken(&sender, reversed).await;
        }
        AACPEvent::ShowNearbyUI { sender } => {
            switcher.on_show_nearby(&sender).await;
        }
        AACPEvent::AudioSource(source) => {
            switcher.on_audio_source(&source.mac, source.r#type).await;
        }
        AACPEvent::StemPress(press, bud) => {
            let press_name = match press {
                StemPressType::SinglePress => "single",
                StemPressType::DoublePress => "double",
                StemPressType::TriplePress => "triple",
                StemPressType::LongPress => "long",
            };
            let bud_name = match bud {
                StemPressBudType::Left => "left",
                StemPressBudType::Right => "right",
            };
            hub.emit_stem_press(address, press_name, bud_name).await;
            if config.stem_control {
                match press {
                    StemPressType::DoublePress => media.next_track().await,
                    StemPressType::TriplePress => media.previous_track().await,
                    _ => {}
                }
            }
        }
        other => debug!("Unhandled AACP event: {:?}", other),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AirPodsInformation {
    pub name: String,
    pub model_number: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub version1: String,
    pub version2: String,
    pub hardware_revision: String,
    pub updater_identifier: String,
    pub left_serial_number: String,
    pub right_serial_number: String,
    pub version3: String,
    pub le_keys: AirPodsLEKeys,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn mode(raw: u8) -> AACPEvent {
        AACPEvent::ControlCommand(ControlCommandStatus { identifier: ControlCommandIdentifiers::ListeningMode, value: vec![raw] })
    }

    fn info() -> AACPEvent {
        AACPEvent::Information(AirPodsInformation::default())
    }

    fn held_raw(a: &AnswerMode) -> Option<u8> {
        a.held.as_ref().map(|(s, _)| s.value[0])
    }

    #[test]
    fn the_answers_mode_waits_for_the_one_after_the_flags() {
        // 11-16 12:50:48: the answer's 01, then 04 once the flags applied
        let now = Instant::now();
        let mut a = AnswerMode::default();
        assert!(a.admit(&info(), now));
        assert!(!a.admit(&mode(0x01), now));
        assert_eq!(held_raw(&a), Some(0x01));
        assert!(a.admit(&mode(0x04), now));
        assert!(a.held.is_none() && a.release().is_none());
        // Reports after that pass straight through
        assert!(a.admit(&mode(0x03), now));
    }

    #[test]
    fn the_answers_mode_stands_if_nothing_follows() {
        let now = Instant::now();
        let mut a = AnswerMode::default();
        a.admit(&info(), now);
        assert!(!a.admit(&mode(0x02), now));
        assert_eq!(a.deadline(), Some(now + ANSWER_MODE_HOLD));
        assert!(matches!(a.release(), Some(AACPEvent::ControlCommand(s)) if s.value == [0x02]));
        assert!(a.deadline().is_none());
    }

    #[test]
    fn only_the_mode_right_after_the_information_is_held() {
        let now = Instant::now();
        let mut a = AnswerMode::default();
        // No answer yet: a report is a report
        assert!(a.admit(&mode(0x04), now));
        // An answer that goes on without a listening mode
        a.admit(&info(), now);
        let other = AACPEvent::ControlCommand(ControlCommandStatus {
            identifier: ControlCommandIdentifiers::ClickHoldInterval,
            value: vec![0],
        });
        assert!(a.admit(&other, now));
        assert!(a.admit(&mode(0x03), now));
        assert!(a.held.is_none());
    }
}
