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
}

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
                },
            );
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
