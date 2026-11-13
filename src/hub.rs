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

    /// The audio profile stays as it is. Turning it back on here, as the link
    /// went down, only worked when it beat the audio card's going (once in
    /// five, 11-13), and then WirePlumber remembered "on": the next connection
    /// came up with the audio on while the iPhone had it, a stream started
    /// here, and the AirPods gave this computer the audio (23:12:45). The
    /// next session sets the profile once it knows who has the audio
    /// (Switcher::on_card); only the daemon's exit turns it back on.
    pub async fn stop(self) {
        self.media.stop().await;
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
    /// When this opening of the lid last asked BlueZ to connect
    lid_connect_at: Option<std::time::Instant>,
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
    /// This connection's end was already noted (by the channel closing or by
    /// BlueZ, whichever came first), and whether it was a release: the
    /// second notice must give the same answer, not overwrite it
    end_noted: Option<bool>,
    /// BlueZ's reason for the end (true: the AirPods ended it), when it came
    /// before the end was noted
    reason: Option<(std::time::Instant, bool)>,
    /// Whether to reconnect was decided for this end (and a round started)
    settled: Option<bool>,
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

/// Control channels opened again on one link, for any reason (Hub::reopen_channel,
/// Hub::retry_channel)
const MAX_REOPENS: u8 = 2;

/// A join this long before the link ended counts as its cause
const JOIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl Entry {
    /// Called as the connection ends, before its live state is cleared. True
    /// when it ended in a release: Reconnecting is then set along with the
    /// clearing, since BlueZ's reason only comes a moment later and a shell
    /// must never see the AirPods gone in between.
    fn note_end(&mut self) -> bool {
        // 18:31:44: BlueZ's notice came first, after the channel's had reset
        // the join, and overwrote "released" with "not": no Reconnecting with
        // the clearing, and the round only started by luck of timing
        if let Some(released) = self.end_noted {
            return released;
        }
        if !self.state.ready {
            self.end_noted = Some(false);
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
        self.end_noted = Some(release.is_some());
        release.is_some()
    }

    /// The end noted as the channel closes or BlueZ reports the link gone
    fn noted_end(&mut self) -> End {
        let released = self.note_end();
        if let Some(reconnecting) = self.settled {
            return End { reconnecting, decision: None, waiting: false };
        }
        match self.reason.take().filter(|(at, _)| at.elapsed() < REASON_WAIT) {
            Some((_, remote)) => {
                let decision = self.decide(remote);
                End { reconnecting: decision.is_some(), decision: Some(decision), waiting: false }
            }
            // Reconnecting shows from now if it's a release: BlueZ's reason
            // only comes a moment later, and shells mustn't see them gone
            None => End { reconnecting: released, decision: None, waiting: released },
        }
    }

    /// BlueZ's reason came. It can come before the end is noted (it arrives
    /// apart from the Connected change, which podsd follows only after reading
    /// the device's properties, and the channel's closing can wait behind
    /// events): then it's kept and decides once the end is noted. Some
    /// decision once both are known.
    fn reason_came(&mut self, remote: bool) -> Option<Option<(u32, Release)>> {
        if self.end_noted.is_none() {
            self.reason = Some((std::time::Instant::now(), remote));
            return None;
        }
        if self.settled.is_some() {
            return None;
        }
        Some(self.decide(remote))
    }

    /// Whether to reconnect, now the end and BlueZ's reason are both known:
    /// only when the AirPods ended the link in a release, and at most so many
    /// rounds a minute so a release right after each reconnect can't loop.
    /// A new round's number and the release.
    fn decide(&mut self, remote: bool) -> Option<(u32, Release)> {
        let decision = self.decide_inner(remote);
        self.settled = Some(decision.is_some());
        decision
    }

    fn decide_inner(&mut self, remote: bool) -> Option<(u32, Release)> {
        const ROUNDS_PER_MINUTE: usize = 3;
        let release = match self.ended.take() {
            Some((at, Some(release))) if remote && at.elapsed() < REASON_WAIT => release,
            _ => return None,
        };
        let now = std::time::Instant::now();
        self.reconnects.retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(60));
        if self.reconnects.len() >= ROUNDS_PER_MINUTE {
            info!(
                "{}: released again, but reconnected {} times this minute already; leaving it",
                self.state.address, ROUNDS_PER_MINUTE
            );
            return None;
        }
        self.reconnects.push(now);
        self.reconnect_round += 1;
        self.active_round = Some(self.reconnect_round);
        self.back_from_join = release == Release::JoinedWhileUnlisted;
        Some((self.reconnect_round, release))
    }
}

/// A connection's end as noted (Entry::noted_end)
struct End {
    /// Reconnecting shows along with the state being cleared
    reconnecting: bool,
    /// Whether to reconnect, if decided just now
    decision: Option<Option<(u32, Release)>>,
    /// A release, with BlueZ's reason still to come
    waiting: bool,
}

/// The end of a connection and BlueZ's reason for it count together this
/// long apart at most
const REASON_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

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
                    lid_connect_at: None,
                    ended: None,
                    reconnects: Vec::new(),
                    reconnect_round: 0,
                    active_round: None,
                    reopens: 0,
                    unlisted_here: false,
                    joining_at: None,
                    other_was_here: false,
                    end_noted: None,
                    reason: None,
                    settled: None,
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
        let (news, open, connect) = {
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
            let open = entry.lid_open && entry.near;
            // Once per opening of the lid, again after 10 s if that didn't
            // connect (the AirPods may have been busy). Without Apple's
            // Device ID the AirPods take one host at a time, so only while
            // they're free; with it they keep this computer alongside the
            // iPhone, which often connects first (11-13 23:13:35, 4 s after
            // the lid opened, before it read as open here): connect anyway.
            let may_join = advert.connection == 0x00 || self.0.apple_id;
            let due = entry.lid_connect_at.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
            let connect = config.auto_connect && open && may_join && !entry.state.connected && due;
            if connect {
                entry.lid_connect_at = Some(std::time::Instant::now());
            }
            if !open {
                entry.lid_connect_at = None;
            }
            (news, open, connect)
        };
        debug!(
            "Advert {} from {}: lid {:02x} status {:02x} connection {:02x} rssi {:?} news {:?} -> open {} | L {:?} R {:?} C {:?}",
            address, advert.source, advert.lid, advert.status, advert.connection, advert.rssi, news, open,
            advert.left, advert.right, advert.case
        );

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
        if connect {
            info!("{}: lid open, connecting", address);
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
            // A new connection: its end is still to come
            entry.end_noted = None;
            entry.reason = None;
            entry.settled = None;
        }
        let hub = self.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            let session = airpods::start(hub.clone(), address.clone()).await;
            let failed = session.is_none();
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
            if failed {
                hub.retry_channel(&address);
            }
        });
    }

    /// The control channel is gone (it closed, or opening it failed) while
    /// BlueZ may still have the AirPods connected: nothing would open it again
    /// until the link went down. If the link is still up in a moment, open it
    /// again, within the link's reopens.
    fn retry_channel(&self, address: &str) {
        let hub = self.clone();
        let address = address.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let retry = hub.0.devices.lock().await.get_mut(&address).is_some_and(|e| {
                let due = e.state.connected && e.session.is_none() && !e.attaching;
                if due && e.reopens >= MAX_REOPENS {
                    info!("{}: no control channel, but opened it {} times on this link already; leaving it", address, MAX_REOPENS);
                }
                let retry = due && e.reopens < MAX_REOPENS;
                if retry {
                    e.reopens += 1;
                }
                retry
            });
            if retry {
                info!("{}: still connected without a control channel; opening it again", address);
                hub.attach(&address).await;
            }
        });
    }

    /// BlueZ disconnected the device.
    pub async fn on_disconnected(&self, address: &str) {
        let (session, end) = {
            let mut devices = self.0.devices.lock().await;
            match devices.get_mut(address) {
                Some(e) => (e.session.take(), e.noted_end()),
                None => (None, End { reconnecting: false, decision: None, waiting: false }),
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
            s.reconnecting |= end.reconnecting;
        })
        .await;
        self.after_end(address, end).await;
    }

    /// The control channel closed while BlueZ may still have the device connected.
    /// Must not run on one of the session's own tasks: stopping the session aborts them.
    pub async fn on_channel_closed(&self, address: &str) {
        let (session, end) = {
            let mut devices = self.0.devices.lock().await;
            match devices.get_mut(address) {
                Some(e) => (e.session.take(), e.noted_end()),
                None => (None, End { reconnecting: false, decision: None, waiting: false }),
            }
        };
        if let Some(session) = session {
            info!("{}: control channel closed", address);
            session.stop().await;
        }
        self.update(address, |s| {
            s.clear_live();
            s.reconnecting |= end.reconnecting;
        })
        .await;
        self.after_end(address, end).await;
        self.retry_channel(address);
    }

    /// BlueZ's reason for the end of a connection (Device1.Disconnected);
    /// `remote`: the AirPods ended it ("Power Off" reads as Remote). After a
    /// lost link (Timeout) BlueZ reconnects by itself.
    pub async fn on_disconnect_reason(&self, address: &str, remote: bool) {
        let decision = self.0.devices.lock().await.get_mut(address).and_then(|e| e.reason_came(remote));
        if let Some(decision) = decision {
            self.act_on(address, decision).await;
        }
    }

    async fn after_end(&self, address: &str, end: End) {
        match end.decision {
            Some(decision) => self.act_on(address, decision).await,
            // Reconnecting was set and BlueZ's reason is still to come: if it
            // never does (the channel closed alone), don't leave it on
            None if end.waiting => {
                let hub = self.clone();
                let address = address.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let idle = hub.0.devices.lock().await.get_mut(&address).is_some_and(|e| {
                        e.ended = None;
                        e.active_round.is_none()
                    });
                    if idle {
                        hub.update(&address, |s| s.reconnecting = false).await;
                    }
                });
            }
            None => {}
        }
    }

    /// Starts the reconnect round decided on, or with none, clears
    /// Reconnecting unless a round is running
    async fn act_on(&self, address: &str, decision: Option<(u32, Release)>) {
        match decision {
            Some((round, release)) => {
                tokio::spawn(reconnect_after_release(self.clone(), address.to_string(), round, release));
            }
            None => {
                let idle = self.0.devices.lock().await.get(address).is_some_and(|e| e.active_round.is_none());
                if idle {
                    self.update(address, |s| s.reconnecting = false).await;
                }
            }
        }
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
            // Don't leave the AirPods silent once nothing manages them
            session.media.restore_profile_if_off().await;
            session.stop().await;
        }
    }
}

/// The AirPods let this computer go while another device had the audio (the
/// main AirPod left an ear, or both were out a while). They do the
/// same to an iPhone in the mirror case, and it reconnects 1–2 s later; BlueZ
/// doesn't reconnect a link the other side ended, so podsd does, as the
/// iPhone would. A few tries only (EarPort: 2 s, then every 5 s, 3 tries).
async fn reconnect_after_release(hub: Hub, address: String, round: u32, release: Release) {
    match release {
        Release::ForOther => info!(
            "{}: the AirPods let this computer go while another device had the audio; connecting again",
            address
        ),
        Release::JoinedWhileUnlisted => {
            info!(
                "{}: the AirPods dropped this computer for a device joining before it was on their priority list; connecting again",
                address
            );
            // That device took the AirPods to play: what played here stops,
            // as when another device takes them over (Apple: the Mac pauses)
            let paused = tokio::task::spawn_blocking(crate::media_controller::pause_local_players_sync)
                .await
                .unwrap_or(0);
            if paused > 0 {
                info!("{}: paused {} player(s): the audio moved to the device that joined", address, paused);
            }
        }
    }
    // Reconnecting was set as the connection ended (Hub::note_end)
    if reconnect(&hub, &address).await {
        // Ready (which clears Reconnecting) follows once the control channel
        // is up; never leave a shell showing AirPods that didn't come back
        let mut answered = false;
        for _ in 0..20 {
            if hub.state(&address).await.is_none_or(|s| s.ready) {
                answered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        if !answered {
            info!("{}: connected again, but the AirPods didn't answer", address);
        }
    } else {
        info!("{}: not reconnecting any more", address);
        // No session to report the move to: a later connection mustn't
        hub.take_back_from_join(&address).await;
    }
    hub.end_reconnect_round(&address, round).await;
}

/// Connects again: after 2 s, then 5 s and 5 s (EarPort's timing). True once
/// connected.
async fn reconnect(hub: &Hub, address: &str) -> bool {
    for (attempt, wait) in [2u64, 5, 5].into_iter().enumerate() {
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        if hub.state(address).await.is_some_and(|s| s.connected) {
            info!("{}: connected again", address);
            return true;
        }
        let Some(path) = hub.bluez_path(address).await else {
            return false;
        };
        // An attempt at AirPods that went off can hang for most of a minute
        // (50 s seen), holding Reconnecting the whole time
        let call = hub
            .system()
            .call_method(Some("org.bluez"), path.as_str(), Some("org.bluez.Device1"), "Connect", &());
        match tokio::time::timeout(std::time::Duration::from_secs(10), call).await {
            Ok(Ok(_)) => {
                info!("{}: connected again (try {})", address, attempt + 1);
                return true;
            }
            Ok(Err(e)) => info!("{}: reconnecting failed (try {}): {}", address, attempt + 1, e),
            Err(_) => info!("{}: reconnecting failed (try {}): no answer in 10 s", address, attempt + 1),
        }
    }
    false
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
            lid_connect_at: None,
            ended: None,
            reconnects: Vec::new(),
            reconnect_round: 0,
            active_round: None,
            reopens: 0,
            unlisted_here: unlisted,
            joining_at: joined_ago.map(|s| std::time::Instant::now() - std::time::Duration::from_millis(s)),
            other_was_here: false,
            end_noted: None,
            reason: None,
            settled: None,
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
    fn the_second_notice_of_an_end_agrees_with_the_first() {
        let mut e = entry(true, true, Some(400));
        assert!(e.note_end());
        // BlueZ's notice after the channel's (or the other way round)
        assert!(e.note_end());
        assert_eq!(e.ended.and_then(|(_, r)| r), Some(Release::JoinedWhileUnlisted));
        let mut quiet = entry(true, false, None);
        assert!(!quiet.note_end());
        assert!(!quiet.note_end());
    }

    fn released() -> Entry {
        let mut e = entry(false, false, None);
        e.state.other_device = "iPhone".into();
        e
    }

    #[test]
    fn the_reason_decides_whichever_comes_first() {
        // BlueZ's reason after the end: Reconnecting at once, the round then
        let mut e = released();
        let end = e.noted_end();
        assert!(end.reconnecting && end.waiting && end.decision.is_none());
        assert!(matches!(e.reason_came(true), Some(Some((1, Release::ForOther)))));
        // ... and the second notice of the end starts nothing more
        let end = e.noted_end();
        assert!(end.reconnecting && end.decision.is_none() && !end.waiting);

        // The reason first: kept, and the end decides
        let mut e = released();
        assert_eq!(e.reason_came(true), None);
        let end = e.noted_end();
        assert!(end.reconnecting && matches!(end.decision, Some(Some((1, Release::ForOther)))));
        assert!(e.noted_end().decision.is_none());

        // A lost link (Timeout) first: no Reconnecting, now or at the second notice
        let mut e = released();
        assert_eq!(e.reason_came(false), None);
        let end = e.noted_end();
        assert!(!end.reconnecting && matches!(end.decision, Some(None)));
        let end = e.noted_end();
        assert!(!end.reconnecting && end.decision.is_none() && !end.waiting);

        // Not a release: the AirPods ending it starts nothing
        let mut e = entry(true, false, None);
        assert!(!e.noted_end().reconnecting);
        assert!(matches!(e.reason_came(true), Some(None)));
    }

    #[test]
    fn a_release_for_another_device_stays_its_own_kind() {
        let mut e = entry(false, false, None);
        e.state.other_device = "iPhone".into();
        assert_eq!(ended(e), Some(Release::ForOther));
    }
}
