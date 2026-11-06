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
    pub tasks: Vec<JoinHandle<()>>,
}

impl Session {
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
    devices: Mutex<HashMap<String, Entry>>,
}

impl Hub {
    pub fn new(conn: Connection, system: Connection, config: Config, local_mac: String) -> Self {
        Hub(Arc::new(Inner {
            conn,
            system,
            config,
            local_mac,
            devices: Mutex::new(HashMap::new()),
        }))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
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
        let session = {
            let mut devices = self.0.devices.lock().await;
            devices.get_mut(address).and_then(|e| e.session.take())
        };
        if let Some(session) = session {
            info!("{} disconnected", address);
            session.stop().await;
        }
        self.update(address, |s| {
            s.connected = false;
            s.clear_live();
        })
        .await;
    }

    /// The control channel closed while BlueZ may still have the device connected.
    /// Must not run on one of the session's own tasks: stopping the session aborts them.
    pub async fn on_channel_closed(&self, address: &str) {
        let session = {
            let mut devices = self.0.devices.lock().await;
            devices.get_mut(address).and_then(|e| e.session.take())
        };
        if let Some(session) = session {
            info!("{}: control channel closed", address);
            session.stop().await;
        }
        self.update(address, |s| s.clear_live()).await;
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
