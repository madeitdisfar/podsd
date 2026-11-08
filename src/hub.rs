//! The daemon's model: every known pair of AirPods, its state, and the session
//! talking to it while it's connected. State changes go through `update`, which
//! announces them on D-Bus.

use crate::api::{DEVICE_IFACE, device_path};
use crate::bluetooth::aacp::AACPManager;
use crate::config::Config;
use crate::dbus_api::DeviceIface;
use crate::devices::airpods;
use crate::media_controller::MediaController;
use crate::state::{DeviceState, diff};
use log::{debug, info, warn};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use zbus::Connection;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Value;

/// The live connection to one pair of AirPods.
pub struct Session {
    pub aacp: AACPManager,
    pub media: MediaController,
    pub switcher: Option<crate::switching::Switcher>,
    pub tasks: Vec<JoinHandle<()>>,
}

impl Session {
    /// Only the channel goes, for a new one on the same link: the audio stays
    /// as it is, and the tasks go first so the closing isn't taken for the
    /// link going down.
    async fn stop_for_reopen(self) {
        for task in self.tasks {
            task.abort();
        }
        self.media.stop().await;
        self.aacp.shutdown().await;
    }

    pub async fn stop(self) {
        self.media.stop().await;
        // Don't leave the AirPods silent once nothing manages them.
        self.media.restore_profile_if_off().await;
        self.aacp.shutdown().await;
        for task in self.tasks {
            task.abort();
        }
    }
}

struct Entry {
    state: DeviceState,
    bluez_path: String,
    session: Option<Session>,
    attaching: bool,
    /// Per advertising bud: its last lid byte and when it was last heard
    lids: HashMap<bluer::Address, (u8, std::time::Instant)>,
    /// The lid is open, from the last change in a bud's lid byte
    lid_open: bool,
    /// While open: an advert was strong enough (caseOpenRssi) to count as near
    near: bool,
    /// When the last connection ended, and whether (and why) the AirPods had
    /// just let this computer go
    ended: Option<(std::time::Instant, Option<Release>)>,
    /// The AirPods' last device list: this computer's entry lacked the
    /// "listed" flag (no priority list sent yet)
    unlisted_here: bool,
    /// When that list last showed another device still connecting
    joining_at: Option<std::time::Instant>,
    /// Another device has been fully connected at some point on this link:
    /// one joining then is rejoining (B19), not arriving to play
    other_was_here: bool,
    /// A reconnect after a JoinedWhileUnlisted release: the device that joined
    /// took the audio, which the next session reports as a move
    back_from_join: bool,
    /// When podsd started reconnecting after such releases, this last minute
    reconnects: Vec<std::time::Instant>,
    /// Control channels reopened on this link (Hub::reopen_channel)
    reopens: u8,
    /// Counts reconnect rounds; the one running, if any
    reconnect_round: u32,
    active_round: Option<u32>,
}

/// Why the AirPods let this computer go, when they expect it back
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Release {
    /// Another device has the audio and the main AirPod left an ear
    /// (DeviceState::released_for_other)
    ForOther,
    /// This computer connected first, alone, so it was never put on the
    /// priority list; another device joining for the first time on this link
    /// (an iPhone picking the AirPods as its output) made the AirPods drop it
    /// 0.2–0.4 s later (2026-11-08 13:54:06, 16:50:23). Back, the list can be
    /// sent; the audio stays with the device that joined for it.
    JoinedWhileUnlisted,
}

/// A join this long before the link ended counts as its cause
const JOIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl Entry {
    /// Called as the connection ends, before its live state is cleared. True
    /// when it ended in a release: Reconnecting is then set along with the
    /// clearing, since BlueZ's reason only comes a moment later and a shell
    /// must never see the AirPods gone in between.
    fn note_end(&mut self) -> bool {
        if !self.state.ready {
            return false;
        }
        let joined = self.joining_at.is_some_and(|t| t.elapsed() < JOIN_WINDOW);
        let release = if self.state.released_for_other() {
            Some(Release::ForOther)
        } else if self.state.owns_audio && self.unlisted_here && joined && !self.other_was_here {
            Some(Release::JoinedWhileUnlisted)
        } else {
            None
        };
        self.ended = Some((std::time::Instant::now(), release));
        self.unlisted_here = false;
        self.joining_at = None;
        self.other_was_here = false;
        release.is_some()
    }
}

/// BlueZ passes an advert on only when it changes, so an open lid that nothing
/// happens to can be quiet for a minute (41 s seen); closing it sends its own
/// advert. This long without any sign of the case, it went out of range.
const ADVERT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Clone)]
pub struct Hub(Arc<Inner>);

struct Inner {
    conn: Connection,
    system: Connection,
    config: Config,
    local_mac: String,
    /// The adapter shows Apple's Device ID (bluetooth:v004C…): the AirPods then
    /// share themselves with an Apple device instead of dropping it
    apple_id: bool,
    devices: Mutex<HashMap<String, Entry>>,
}

impl Hub {
    pub fn new(conn: Connection, system: Connection, config: Config, local_mac: String, apple_id: bool) -> Self {
        Hub(Arc::new(Inner {
            conn,
            system,
            config,
            local_mac,
            apple_id,
            devices: Mutex::new(HashMap::new()),
        }))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn apple_id(&self) -> bool {
        self.0.apple_id
    }

    /// Takes the AirPods over by hand (Move back, "Use here")
    pub async fn move_here(&self, address: &str) -> bool {
        let switcher = {
            let devices = self.0.devices.lock().await;
            devices
                .get(address)
                .and_then(|e| e.session.as_ref())
                .and_then(|s| s.switcher.clone())
        };
        match switcher {
            Some(s) => {
                s.take_over(true).await;
                true
            }
            None => false,
        }
    }

    pub async fn emit_moved_to(&self, address: &str, name: &str, reversed: bool) {
        let path = device_path(address);
        if let Ok(emitter) = SignalEmitter::new(&self.0.conn, path.as_str())
            && let Err(e) = DeviceIface::moved_to(&emitter, name, reversed).await
        {
            warn!("Failed to emit MovedTo: {}", e);
        }
    }

    pub fn local_mac(&self) -> &str {
        &self.0.local_mac
    }

    pub fn system(&self) -> &Connection {
        &self.0.system
    }

    /// Adds a device (once) and its D-Bus object.
    pub async fn add_device(&self, address: &str, name: &str, bluez_path: &str) {
        {
            let mut devices = self.0.devices.lock().await;
            if let Some(entry) = devices.get_mut(address) {
                if !bluez_path.is_empty() {
                    entry.bluez_path = bluez_path.to_string();
                }
                return;
            }
            devices.insert(
                address.to_string(),
                Entry {
                    state: DeviceState::new(address, name),
                    bluez_path: bluez_path.to_string(),
                    session: None,
                    attaching: false,
                    lids: HashMap::new(),
                    lid_open: false,
                    near: false,
                    ended: None,
                    reconnects: Vec::new(),
                    reconnect_round: 0,
                    active_round: None,
                    reopens: 0,
                    unlisted_here: false,
                    joining_at: None,
                    other_was_here: false,
                    back_from_join: false,
                },
            );
            // The case's last reading, from an earlier run
            if let Some((level, time)) = crate::utils::load_case_batteries().get(address)
                && let Some(entry) = devices.get_mut(address)
            {
                entry.state.battery_case = crate::state::Battery {
                    level: *level,
                    charging: false,
                    live: false,
                    time: *time,
                };
            }
        }
        info!("Known AirPods: {} ({})", name, address);
        let iface = DeviceIface::new(self.clone(), address.to_string());
        if let Err(e) = self.0.conn.object_server().at(device_path(address), iface).await {
            warn!("Failed to export {} on D-Bus: {}", address, e);
        }
    }

    /// Shows the AirPods' own name in BlueZ (and so in every Bluetooth list and
    /// PipeWire's device name): over classic Bluetooth they call themselves
    /// "<name> - Find My". Only while the alias was never set by hand (it equals
    /// BlueZ's Name), or when `force` (a rename through podsd).
    pub async fn sync_alias(&self, address: &str, name: &str, force: bool) {
        let Some(path) = self.bluez_path(address).await else {
            return;
        };
        let system = &self.0.system;
        let get = |prop: &'static str| {
            let path = path.clone();
            async move {
                system
                    .call_method(
                        Some("org.bluez"),
                        path.as_str(),
                        Some("org.freedesktop.DBus.Properties"),
                        "Get",
                        &("org.bluez.Device1", prop),
                    )
                    .await
                    .ok()
                    .and_then(|reply| reply.body().deserialize::<zbus::zvariant::OwnedValue>().ok())
                    .and_then(|v| v.downcast_ref::<&str>().ok().map(|s| s.to_string()))
            }
        };
        let (Some(bluez_name), Some(alias)) = (get("Name").await, get("Alias").await) else {
            return;
        };
        if name.is_empty() || alias == name || (!force && alias != bluez_name) {
            return;
        }
        info!("{}: Bluetooth alias {:?} -> {:?}", address, alias, name);
        if let Err(e) = system
            .call_method(
                Some("org.bluez"),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &("org.bluez.Device1", "Alias", Value::from(name)),
            )
            .await
        {
            warn!("Failed to set the alias of {}: {}", address, e);
        }
    }

    /// A BLE advert from this pair (bluetooth/le.rs).
    pub async fn on_advert(&self, address: &str, advert: crate::bluetooth::le::Advert) {
        let config = &self.0.config;
        let strong = advert.rssi.is_none_or(|r| r >= config.case_open_rssi);
        // Both buds advertise, alternately, and a bud outside the case keeps
        // repeating a stale lid byte: only a change in a bud's lid byte is news
        // about the lid, and only while a bud is in the case. Nearness is apart:
        // the first advert after opening can be weak, and later ones only repeat
        // it. Once near, it stays near until the lid closes, so the signal
        // strength wobbling around the threshold doesn't close the card.
        let (news, open) = {
            let mut devices = self.0.devices.lock().await;
            let Some(entry) = devices.get_mut(address) else {
                return;
            };
            let previous = entry
                .lids
                .insert(advert.source, (advert.lid, std::time::Instant::now()))
                .map(|(lid, _)| lid);
            let news = (previous != Some(advert.lid) && advert.in_case()).then(|| advert.lid_open());
            if let Some(open) = news {
                entry.lid_open = open;
            }
            // Both buds in ears (status 0x02 and 0x08): nothing in the case can
            // tell when it closes, and they're in use: the card's moment is over
            if advert.status & 0x0a == 0x0a {
                entry.lid_open = false;
            }
            entry.near = entry.lid_open && (entry.near || strong);
            (news, entry.lid_open && entry.near)
        };
        debug!(
            "Advert {} from {}: lid {:02x} status {:02x} connection {:02x} rssi {:?} news {:?} -> open {} | L {:?} R {:?} C {:?}",
            address, advert.source, advert.lid, advert.status, advert.connection, advert.rssi, news, open,
            advert.left, advert.right, advert.case
        );

        let connected = self.state(address).await.is_some_and(|s| s.connected);
        let reading = |p: Option<(i32, bool)>| {
            p.map(|(l, c)| crate::state::Battery::reading(l, c))
                .unwrap_or_default()
        };
        self.update(address, |s| {
            s.case_open = open;
            if let Some((level, charging)) = advert.case {
                s.battery_case = crate::state::Battery::reading(level, charging);
            }
            // The control channel's reports are exact; adverts fill in without it
            if !s.ready && (advert.left.is_some() || advert.right.is_some()) {
                s.battery_left = reading(advert.left);
                s.battery_right = reading(advert.right);
                s.battery_source = "ble".to_string();
            }
        })
        .await;
        if let Some((level, _)) = advert.case {
            crate::utils::save_case_battery(address, level, crate::state::now());
        }
        if config.auto_connect && open && !connected && advert.connection == 0x00 {
            info!("{}: lid open and free, connecting", address);
            if let Some(path) = self.bluez_path(address).await {
                let system = self.0.system.clone();
                tokio::spawn(async move {
                    let _ = system
                        .call_method(Some("org.bluez"), path.as_str(), Some("org.bluez.Device1"), "Connect", &())
                        .await;
                });
            }
        }
    }

    /// A sign of a bud without a new advert (its signal strength changed)
    pub async fn on_advert_seen(&self, address: &str, source: bluer::Address) {
        if let Some(entry) = self.0.devices.lock().await.get_mut(address)
            && let Some(lid) = entry.lids.get_mut(&source)
        {
            lid.1 = std::time::Instant::now();
        }
    }

    /// One bud's adverts stopped: the case is gone once no bud is heard
    pub async fn on_advert_lost(&self, address: &str, source: bluer::Address) {
        let gone = {
            let mut devices = self.0.devices.lock().await;
            let Some(entry) = devices.get_mut(address) else {
                return;
            };
            entry.lids.remove(&source);
            entry.lids.values().all(|(_, heard)| heard.elapsed() > ADVERT_TIMEOUT)
        };
        if gone {
            self.close_lid(address).await;
        }
    }

    async fn close_lid(&self, address: &str) {
        if let Some(entry) = self.0.devices.lock().await.get_mut(address) {
            entry.lid_open = false;
            entry.near = false;
        }
        self.update(address, |s| s.case_open = false).await;
    }

    /// Ends CaseOpen once adverts stop.
    pub fn start_advert_timeout(&self) {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let stale: Vec<String> = {
                    let devices = hub.0.devices.lock().await;
                    devices
                        .iter()
                        .filter(|(_, e)| {
                            e.lid_open
                                && e.lids.values().all(|(_, heard)| heard.elapsed() > ADVERT_TIMEOUT)
                        })
                        .map(|(a, _)| a.clone())
                        .collect()
                };
                for address in stale {
                    hub.close_lid(&address).await;
                }
            }
        });
    }

    pub async fn has_device(&self, address: &str) -> bool {
        self.0.devices.lock().await.contains_key(address)
    }

    pub async fn state(&self, address: &str) -> Option<DeviceState> {
        self.0.devices.lock().await.get(address).map(|e| e.state.clone())
    }

    pub async fn bluez_path(&self, address: &str) -> Option<String> {
        self.0
            .devices
            .lock()
            .await
            .get(address)
            .map(|e| e.bluez_path.clone())
            .filter(|p| !p.is_empty())
    }

    pub async fn aacp(&self, address: &str) -> Option<AACPManager> {
        self.0
            .devices
            .lock()
            .await
            .get(address)
            .and_then(|e| e.session.as_ref().map(|s| s.aacp.clone()))
    }

    /// Changes a device's state and announces the properties that changed.
    pub async fn update<F: FnOnce(&mut DeviceState)>(&self, address: &str, f: F) {
        let (old, new) = {
            let mut devices = self.0.devices.lock().await;
            let Some(entry) = devices.get_mut(address) else {
                return;
            };
            let old = entry.state.clone();
            f(&mut entry.state);
            (old, entry.state.clone())
        };
        let changes = diff(&old, &new);
        if changes.is_empty() {
            return;
        }
        debug!(
            "{}: {}",
            address,
            changes.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
        );
        let changed: HashMap<&str, Value> = changes.iter().map(|(k, p)| (*k, p.to_value())).collect();
        let path = device_path(address);
        match SignalEmitter::new(&self.0.conn, path.as_str()) {
            Ok(emitter) => {
                if let Err(e) = zbus::fdo::Properties::properties_changed(
                    &emitter,
                    InterfaceName::from_static_str_unchecked(DEVICE_IFACE),
                    changed,
                    Cow::Borrowed(&[]),
                )
                .await
                {
                    warn!("Failed to announce changes of {}: {}", address, e);
                }
            }
            Err(e) => warn!("Bad object path {}: {}", path, e),
        }
    }

    /// BlueZ connected the device: open the control channel unless it is already
    /// open or opening.
    pub async fn on_connected(&self, address: &str) {
        self.update(address, |s| s.connected = true).await;
        self.attach(address).await;
    }

    /// The AirPods took the control channel but say nothing on it: open a new
    /// one on the same link, at most twice per link. The state shown stays as
    /// it is (no Ready change), so shells see the information arrive.
    /// (Boxed: a session reopens through this, and reopening starts a session.)
    pub fn reopen_channel(&self, address: &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let hub = self.clone();
        let address = address.to_string();
        Box::pin(async move { hub.reopen_channel_inner(&address).await })
    }

    async fn reopen_channel_inner(&self, address: &str) {
        const MAX_REOPENS: u8 = 2;
        let session = {
            let mut devices = self.0.devices.lock().await;
            let Some(entry) = devices.get_mut(address) else {
                return;
            };
            if entry.reopens >= MAX_REOPENS {
                info!("{}: reopened the control channel {} times already; leaving it", address, MAX_REOPENS);
                return;
            }
            entry.reopens += 1;
            entry.session.take()
        };
        let Some(session) = session else {
            return;
        };
        session.stop_for_reopen().await;
        if self.state(address).await.is_some_and(|s| s.connected) {
            self.attach(address).await;
        }
    }

    /// Opens the control channel unless it is already open or opening.
    async fn attach(&self, address: &str) {
        {
            let mut devices = self.0.devices.lock().await;
            let Some(entry) = devices.get_mut(address) else {
                return;
            };
            if entry.session.is_some() || entry.attaching {
                return;
            }
            entry.attaching = true;
        }
        let hub = self.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            let session = airpods::start(hub.clone(), address.clone()).await;
            let leftover = {
                let mut devices = hub.0.devices.lock().await;
                match devices.get_mut(&address) {
                    Some(entry) => {
                        entry.attaching = false;
                        if entry.state.connected {
                            entry.session = session;
                            None
                        } else {
                            session
                        }
                    }
                    None => session,
                }
            };
            // Disconnected while the channel was opening.
            if let Some(session) = leftover {
                session.stop().await;
                hub.update(&address, |s| s.clear_live()).await;
            }
        });
    }

    /// BlueZ disconnected the device.
    pub async fn on_disconnected(&self, address: &str) {
        let (session, released) = {
            let mut devices = self.0.devices.lock().await;
            match devices.get_mut(address) {
                Some(e) => (e.session.take(), e.note_end()),
                None => (None, false),
            }
        };
        if let Some(session) = session {
            info!("{} disconnected", address);
            session.stop().await;
        }
        if let Some(e) = self.0.devices.lock().await.get_mut(address) {
            e.reopens = 0;
        }
        self.update(address, |s| {
            s.connected = false;
            s.clear_live();
            s.reconnecting |= released;
        })
        .await;
        if released {
            self.expire_unconfirmed_release(address);
        }
    }

    /// The control channel closed while BlueZ may still have the device connected.
    /// Must not run on one of the session's own tasks: stopping the session aborts them.
    pub async fn on_channel_closed(&self, address: &str) {
        let (session, released) = {
            let mut devices = self.0.devices.lock().await;
            match devices.get_mut(address) {
                Some(e) => (e.session.take(), e.note_end()),
                None => (None, false),
            }
        };
        if let Some(session) = session {
            info!("{}: control channel closed", address);
            session.stop().await;
        }
        self.update(address, |s| {
            s.clear_live();
            s.reconnecting |= released;
        })
        .await;
        if released {
            self.expire_unconfirmed_release(address);
        }
    }

    /// Reconnecting set as the connection ended, but no reconnect round began
    /// (BlueZ gave another reason, or none came): clear it after a moment.
    fn expire_unconfirmed_release(&self, address: &str) {
        let hub = self.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            hub.drop_unconfirmed_release(&address).await;
        });
    }

    /// The connection ended for another reason than a release: no reconnect
    /// is coming, so clear Reconnecting unless a round is running.
    pub async fn drop_unconfirmed_release(&self, address: &str) {
        let idle = self.0.devices.lock().await.get_mut(address).is_some_and(|e| {
            e.ended = None;
            e.active_round.is_none()
        });
        if idle {
            self.update(address, |s| s.reconnecting = false).await;
        }
    }

    /// The connection that just ended was the AirPods letting this computer
    /// go for another device (released_for_other), and reconnecting is due: a new round,
    /// at most so many a minute so a release right after each reconnect can't
    /// loop. Answers once per disconnect.
    pub async fn start_reconnect_round(&self, address: &str) -> Option<(u32, Release)> {
        const ROUNDS_PER_MINUTE: usize = 3;
        let mut devices = self.0.devices.lock().await;
        let entry = devices.get_mut(address)?;
        let release = match entry.ended.take() {
            Some((at, Some(release))) if at.elapsed() < std::time::Duration::from_secs(5) => release,
            _ => return None,
        };
        let now = std::time::Instant::now();
        entry.reconnects.retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(60));
        if entry.reconnects.len() >= ROUNDS_PER_MINUTE {
            info!("{}: released again, but reconnected {} times this minute already; leaving it", address, ROUNDS_PER_MINUTE);
            return None;
        }
        entry.reconnects.push(now);
        entry.reconnect_round += 1;
        entry.active_round = Some(entry.reconnect_round);
        entry.back_from_join = release == Release::JoinedWhileUnlisted;
        Some((entry.reconnect_round, release))
    }

    /// What the AirPods' device list says about this computer and the others
    /// (for Release::JoinedWhileUnlisted): whether this computer's entry has
    /// the listed flag, and whether another device is still connecting.
    pub async fn note_device_list(
        &self,
        address: &str,
        listed_here: Option<bool>,
        other_joining: bool,
        other_connected: bool,
    ) {
        if let Some(e) = self.0.devices.lock().await.get_mut(address) {
            // No entry for this computer: unknown, so not counted as unlisted
            e.unlisted_here = listed_here == Some(false);
            if other_joining {
                e.joining_at = Some(std::time::Instant::now());
            }
            e.other_was_here |= other_connected;
        }
    }

    /// Whether this connection follows a JoinedWhileUnlisted release. Answers
    /// once.
    pub async fn take_back_from_join(&self, address: &str) -> bool {
        self.0
            .devices
            .lock()
            .await
            .get_mut(address)
            .is_some_and(|e| std::mem::take(&mut e.back_from_join))
    }

    /// The reconnect round is over: clears Reconnecting, unless a newer round
    /// has begun since.
    pub async fn end_reconnect_round(&self, address: &str, round: u32) {
        let latest = self.0.devices.lock().await.get_mut(address).is_some_and(|e| {
            let latest = e.active_round == Some(round);
            if latest {
                e.active_round = None;
            }
            latest
        });
        if latest {
            self.update(address, |s| s.reconnecting = false).await;
        }
    }

    pub async fn emit_stem_press(&self, address: &str, press: &str, bud: &str) {
        let path = device_path(address);
        if let Ok(emitter) = SignalEmitter::new(&self.0.conn, path.as_str())
            && let Err(e) = DeviceIface::stem_press(&emitter, press, bud).await
        {
            warn!("Failed to emit StemPress: {}", e);
        }
    }

    pub async fn shutdown(&self) {
        let sessions: Vec<Session> = {
            let mut devices = self.0.devices.lock().await;
            devices.values_mut().filter_map(|e| e.session.take()).collect()
        };
        for session in sessions {
            session.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(owns: bool, unlisted: bool, joined_ago: Option<u64>) -> Entry {
        let mut state = DeviceState::new("00:00:00:00:00:00", "test");
        state.ready = true;
        state.owns_audio = owns;
        state.ear_primary = "in-ear".into();
        state.ear_secondary = "in-ear".into();
        Entry {
            state,
            bluez_path: String::new(),
            session: None,
            attaching: false,
            lids: HashMap::new(),
            lid_open: false,
            near: false,
            ended: None,
            reconnects: Vec::new(),
            reconnect_round: 0,
            active_round: None,
            reopens: 0,
            unlisted_here: unlisted,
            joining_at: joined_ago.map(|s| std::time::Instant::now() - std::time::Duration::from_millis(s)),
            other_was_here: false,
            back_from_join: false,
        }
    }

    fn ended(mut e: Entry) -> Option<Release> {
        e.note_end();
        e.ended.and_then(|(_, r)| r)
    }

    #[test]
    fn joined_while_unlisted_needs_every_condition() {
        // 2026-11-08 16:50:23: owning, unlisted, the iPhone joining 0.4 s before
        assert_eq!(ended(entry(true, true, Some(400))), Some(Release::JoinedWhileUnlisted));
        // Listed: the list did its job, any drop is something else
        assert_eq!(ended(entry(true, false, Some(400))), None);
        // Nobody joining, or the join too long before
        assert_eq!(ended(entry(true, true, None)), None);
        assert_eq!(ended(entry(true, true, Some(2500))), None);
        // Not owning: that's ForOther's case (another device has the audio)
        assert_eq!(ended(entry(false, true, Some(400))), None);
        // 2026-11-07 13:47:39 (B19): the iPhone had been here and rejoined
        let mut e = entry(true, true, Some(400));
        e.other_was_here = true;
        assert_eq!(ended(e), None);
        // The channel never came up
        let mut e = entry(true, true, Some(400));
        e.state.ready = false;
        assert_eq!(ended(e), None);
    }

    #[test]
    fn a_release_for_another_device_stays_its_own_kind() {
        let mut e = entry(false, false, None);
        e.state.other_device = "iPhone".into();
        assert_eq!(ended(e), Some(Release::ForOther));
    }
}
