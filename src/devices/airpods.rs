//! One session with a pair of AirPods: opens the AACP control channel, then turns
//! its events into state (for D-Bus) and media actions (pause on removal, etc.).

use crate::bluetooth::aacp::{
    AACPEvent, AACPManager, AirPodsLEKeys, BatteryComponent, BatteryStatus,
    ControlCommandIdentifiers, EarDetectionStatus, ProximityKeyType, StemPressBudType,
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
    media.restore_profile_if_off().await;

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
    // their settings back for about 30 s whatever is sent (seen twice: retries
    // at 2, 5, 10 and 20 s were all ignored); clients show the listening mode
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
    // whatever is asked (3 times so far, the last 2026-11-08 16:56: 30 s of
    // silence through three requests; once they dropped this computer after
    // 76 s). A new channel on the same link answers at once.
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

    {
        let hub = hub.clone();
        let media = media.clone();
        let address = address.clone();
        let switcher = switcher.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let AACPEvent::Disconnected = event {
                    // Stopping the session aborts this task, so hand it off.
                    let hub = hub.clone();
                    let address = address.clone();
                    tokio::spawn(async move { hub.on_channel_closed(&address).await });
                    break;
                }
                handle_event(&hub, &media, &switcher, &address, event).await;
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
    media: &MediaController,
    switcher: &Switcher,
    address: &str,
    event: AACPEvent,
) {
    let config = hub.config();
    match event {
        AACPEvent::BatteryInfo(batteries) => {
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
                            s.battery_case = if absent { s.battery_case.kept() } else { battery }
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
        }
        AACPEvent::EarDetection(old, new) => {
            hub.update(address, |s| {
                s.ear_primary = new.first().map(ear_name).unwrap_or_default().to_string();
                s.ear_secondary = new.get(1).map(ear_name).unwrap_or_default().to_string();
            })
            .await;
            let in_ear = |v: &Vec<EarDetectionStatus>| v.contains(&EarDetectionStatus::InEar);
            let (was_in, any_in) = (in_ear(&old), in_ear(&new));
            // The AirPods' own Automatic Ear Detection setting turns this off,
            // as on iOS (pause / resume stop there too). The AirPods never report
            // it, so this is the value set through podsd (or on by default).
            let enabled = hub
                .state(address)
                .await
                .is_some_and(|s| s.settings.get("ear-detection") != Some(&settings::SettingValue::Bool(false)));
            if config.ear_pause && enabled {
                media.handle_ear_detection(old, new).await;
                // After the pause / resume above, so the claim comes with playback
                switcher.on_ear(was_in, any_in).await;
            }
        }
        AACPEvent::ConversationalAwareness(status) => {
            if config.conversation_volume {
                media.handle_conversational_awareness(status).await;
            }
        }
        AACPEvent::Information(info) => {
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
