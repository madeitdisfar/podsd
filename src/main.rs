//! podsd: AirPods daemon. Talks to the AirPods over Bluetooth (the AACP protocol
//! reverse engineered by LibrePods) and serves their state on the session bus.

mod api;
mod bluetooth;
mod config;
mod dbus_api;
mod devices;
mod hub;
mod media_controller;
mod settings;
mod state;
mod switching;
mod utils;

use crate::api::{BUS_NAME, ROOT_PATH};
use crate::config::Config;
use crate::dbus_api::ManagerIface;
use crate::devices::enums::{DeviceInformation, DeviceType};
use crate::hub::Hub;
use clap::Parser;
use futures::StreamExt;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use zbus::fdo::{ObjectManager, ObjectManagerProxy, RequestNameFlags, RequestNameReply};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

/// The AACP service UUID: BlueZ lists it for AirPods (and Beats) only.
const AACP_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

#[derive(Parser)]
#[command(version, about = "AirPods daemon: battery, ear detection, listening modes and settings over D-Bus")]
struct Args {
    /// Log debug messages (includes raw AACP packets).
    #[arg(long, short = 'd')]
    debug: bool,
}

type BluezObjects = HashMap<OwnedObjectPath, HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>>;

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(|v| v.downcast_ref::<&str>().ok().map(|s| s.to_string()))
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| v.downcast_ref::<bool>().ok())
}

fn has_aacp(props: &HashMap<String, OwnedValue>) -> bool {
    props
        .get("UUIDs")
        .and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok())
        .is_some_and(|uuids| uuids.iter().any(|u| u.eq_ignore_ascii_case(AACP_UUID)))
}

async fn bluez_objects(system: &Connection) -> zbus::Result<BluezObjects> {
    let proxy = ObjectManagerProxy::builder(system)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    Ok(proxy.get_managed_objects().await?)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if std::env::var("RUST_LOG").is_err() {
        let level = if args.debug { "debug" } else { "info" };
        // SAFETY: still single-threaded user code; nothing reads the environment concurrently.
        unsafe { std::env::set_var("RUST_LOG", format!("{level},zbus=warn,tracing=warn")) };
    }
    env_logger::init();

    if let Err(e) = run().await {
        error!("{}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load();
    debug!("Config: {:?}", config);

    let system = Connection::system().await?;
    let session = Connection::session().await?;

    let objects = bluez_objects(&system).await?;
    let local_mac = objects
        .values()
        .find_map(|ifaces| {
            ifaces
                .iter()
                .find(|(name, _)| name.as_str() == "org.bluez.Adapter1")
                .and_then(|(_, props)| prop_str(props, "Address"))
        })
        .ok_or("No Bluetooth adapter found")?;

    // Apple's Device ID (DeviceID = bluetooth:004C:… in /etc/bluetooth/main.conf)
    let apple_id = objects
        .values()
        .find_map(|ifaces| {
            ifaces
                .iter()
                .find(|(name, _)| name.as_str() == "org.bluez.Adapter1")
                .and_then(|(_, props)| prop_str(props, "Modalias"))
        })
        .is_some_and(|m| m.to_ascii_lowercase().starts_with("bluetooth:v004c"));
    info!(
        "Device ID: {}",
        if apple_id { "Apple's: automatic switching with Apple devices" } else { "not Apple's: no automatic switching" }
    );
    let hub = Hub::new(session.clone(), system.clone(), config.clone(), local_mac, apple_id);

    session.object_server().at(ROOT_PATH, ObjectManager).await?;
    session.object_server().at(ROOT_PATH, ManagerIface).await?;

    // Known AirPods: paired in BlueZ, plus any remembered from an earlier run.
    let mut connected = Vec::new();
    for (path, ifaces) in &objects {
        let Some((_, props)) = ifaces.iter().find(|(n, _)| n.as_str() == "org.bluez.Device1") else {
            continue;
        };
        if !has_aacp(props) || !prop_bool(props, "Paired").unwrap_or(false) {
            continue;
        }
        let Some(address) = prop_str(props, "Address") else {
            continue;
        };
        let name = prop_str(props, "Alias")
            .or_else(|| prop_str(props, "Name"))
            .unwrap_or_else(|| "AirPods".to_string());
        hub.add_device(&address, &name, path.as_str()).await;
        if prop_bool(props, "Connected").unwrap_or(false) {
            connected.push(address);
        }
    }
    for (address, data) in utils::load_devices() {
        if data.type_ == DeviceType::AirPods && !hub.has_device(&address).await {
            let name = match &data.information {
                Some(DeviceInformation::AirPods(info)) if !info.name.is_empty() => info.name.clone(),
                _ => data.name.clone(),
            };
            hub.add_device(&address, &name, "").await;
        }
    }

    // Serve before taking the name, so a client that sees the name sees the objects.
    match session
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await?
    {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
        _ => return Err(format!("{BUS_NAME} is taken: is podsd already running?").into()),
    }
    info!("podsd {} on the session bus as {}", env!("CARGO_PKG_VERSION"), BUS_NAME);

    for address in connected {
        hub.on_connected(&address).await;
    }

    tokio::spawn(watch_bluez(hub.clone()));
    tokio::spawn(log_disconnect_reasons(hub.clone()));
    hub.start_advert_timeout();

    if config.ble_monitor {
        tokio::spawn(run_le_monitor(hub.clone(), system.clone()));
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("Shutting down");
    hub.shutdown().await;
    Ok(())
}

/// Runs the BLE watcher, and again whenever bluetoothd restarts: a restart drops
/// its advertisement monitors without telling the client, so adverts just stop.
async fn run_le_monitor(hub: Hub, system: Connection) {
    let restarts = async {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, "org.bluez")?
            .build();
        MessageStream::for_match_rule(rule, &system, None).await
    }
    .await;
    let mut restarts = match restarts {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("Can't follow bluetoothd restarts: {}", e);
            None
        }
    };
    loop {
        let hub = hub.clone();
        let monitor = tokio::spawn(async move {
            if let Err(e) = bluetooth::le::start_le_monitor(hub).await {
                warn!("BLE monitor stopped: {} (needs Experimental = true in /etc/bluetooth/main.conf)", e);
            }
        });
        // Wait for bluetoothd to come back under a new owner
        let Some(stream) = restarts.as_mut() else {
            let _ = monitor.await;
            return;
        };
        loop {
            match stream.next().await {
                Some(Ok(msg)) => {
                    if let Ok((_, _, new_owner)) = msg.body().deserialize::<(String, String, String)>()
                        && !new_owner.is_empty()
                    {
                        break;
                    }
                }
                Some(Err(_)) => continue,
                None => {
                    let _ = monitor.await;
                    return;
                }
            }
        }
        info!("bluetoothd restarted, watching for adverts again");
        monitor.abort();
        // Give bluetoothd a moment to set the adapter up
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Logs why AirPods disconnected (BlueZ's Device1.Disconnected: the reason,
/// such as org.bluez.Reason.Remote or .Timeout, and a message)
async fn log_disconnect_reasons(hub: Hub) {
    let system = hub.system().clone();
    let rule = async {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.bluez")?
            .interface("org.bluez.Device1")?
            .member("Disconnected")?
            .path_namespace("/org/bluez")?
            .build();
        MessageStream::for_match_rule(rule, &system, None).await
    }
    .await;
    let mut stream = match rule {
        Ok(s) => s,
        Err(e) => {
            debug!("Can't follow disconnect reasons: {}", e);
            return;
        }
    };
    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        let Some(path) = header.path() else { continue };
        // /org/bluez/hci0/dev_20_52_1D_6F_70_76
        let Some(address) = path.as_str().rsplit('/').next().and_then(|d| d.strip_prefix("dev_")) else {
            continue;
        };
        let address = address.replace('_', ":");
        if !hub.has_device(&address).await {
            continue;
        }
        let (reason, message) = msg.body().deserialize::<(String, String)>().unwrap_or_default();
        info!("{} disconnected: {} ({})", address, reason, message);
        // The AirPods ended it ("Power Off" reads as Remote here). After a
        // lost link (Timeout) BlueZ reconnects by itself.
        if reason == "org.bluez.Reason.Remote" && hub.take_released_for_other(&address).await {
            tokio::spawn(reconnect_after_release(hub.clone(), address));
        }
    }
}

/// Both AirPods came out of the ears while another device had the audio, and
/// the AirPods let this computer go (map B21). They do the same to an iPhone
/// in the mirror case, and it reconnects 1–2 s later; BlueZ doesn't reconnect
/// a link the other side ended, so podsd does, as the iPhone would. A few
/// tries only (EarPort: 2 s, then every 5 s, 3 tries).
async fn reconnect_after_release(hub: Hub, address: String) {
    info!(
        "{}: the AirPods let this computer go as both came out while another device had the audio; connecting again",
        address
    );
    for (attempt, wait) in [2u64, 5, 5].into_iter().enumerate() {
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        if hub.state(&address).await.is_some_and(|s| s.connected) {
            info!("{}: connected again", address);
            return;
        }
        let Some(path) = hub.bluez_path(&address).await else {
            return;
        };
        match hub
            .system()
            .call_method(Some("org.bluez"), path.as_str(), Some("org.bluez.Device1"), "Connect", &())
            .await
        {
            Ok(_) => {
                info!("{}: connected again (try {})", address, attempt + 1);
                return;
            }
            Err(e) => info!("{}: reconnecting failed (try {}): {}", address, attempt + 1, e),
        }
    }
    info!("{}: not reconnecting any more", address);
}

/// Follows BlueZ: AirPods connecting and disconnecting, and newly paired ones.
async fn watch_bluez(hub: Hub) {
    if let Err(e) = watch_bluez_inner(&hub).await {
        error!("Lost BlueZ: {}", e);
        std::process::exit(1);
    }
}

async fn watch_bluez_inner(hub: &Hub) -> zbus::Result<()> {
    let system = hub.system().clone();
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace("/org/bluez")?
        .arg(0, "org.bluez.Device1")?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, &system, None).await?;
    info!("Watching BlueZ for AirPods");

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let header = msg.header();
        let Some(path) = header.path() else { continue };
        let Ok((_, changed, _)) = msg
            .body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            continue;
        };
        let Some(is_connected) = prop_bool(&changed, "Connected") else {
            continue;
        };

        let props: HashMap<String, OwnedValue> = match system
            .call_method(
                Some("org.bluez"),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &("org.bluez.Device1",),
            )
            .await
            .and_then(|reply| reply.body().deserialize::<HashMap<String, OwnedValue>>())
        {
            Ok(p) => p,
            Err(e) => {
                debug!("Can't read {}: {}", path, e);
                continue;
            }
        };
        let Some(address) = prop_str(&props, "Address") else { continue };
        if !hub.has_device(&address).await {
            if !has_aacp(&props) {
                continue;
            }
            let name = prop_str(&props, "Alias").unwrap_or_else(|| "AirPods".to_string());
            hub.add_device(&address, &name, path.as_str()).await;
        } else {
            hub.add_device(&address, "", path.as_str()).await;
        }

        if is_connected {
            info!("{} connected", address);
            hub.on_connected(&address).await;
        } else {
            hub.on_disconnected(&address).await;
        }
    }
    Ok(())
}
