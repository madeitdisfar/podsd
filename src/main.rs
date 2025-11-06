mod bluetooth;
mod media_controller;
mod ui;
mod utils;
mod devices;

use std::env;
use log::info;
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::message::MatchRule;
use dbus::arg::{RefArg, Variant};
use std::collections::HashMap;
use std::sync::Arc;
use crate::bluetooth::discovery::{find_connected_airpods, find_other_managed_devices};
use devices::airpods::AirPodsDevice;
use bluer::Address;
use ksni::TrayMethods;
use crate::ui::tray::MyTray;
use clap::Parser;
use crate::bluetooth::le::start_le_monitor;
use tokio::sync::mpsc::unbounded_channel;
use crate::bluetooth::att::ATTHandles;
use crate::bluetooth::managers::BluetoothManager;
use crate::devices::enums::DeviceData;
use crate::ui::messages::{AirPodsCommand, BluetoothUIMessage, NothingCommand, UICommand};
use crate::utils::get_devices_path;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    debug: bool,
    #[arg(long)]
    no_tray: bool,
    #[arg(long)]
    start_minimized: bool,
}

fn main() -> iced::Result {
    let args = Args::parse();
    let log_level = if args.debug { "debug" } else { "info" };
    if env::var("RUST_LOG").is_err() {
        unsafe { env::set_var("RUST_LOG", log_level.to_owned() + ",iced_wgpu=off,wgpu_hal=off,wgpu_core=off,librepods_rust::bluetooth::le=off,cosmic_text=off,naga=off,iced_winit=off") };
    }
    env_logger::init();

    let (ui_tx, ui_rx) = unbounded_channel::<BluetoothUIMessage>();
    let (ui_command_tx, ui_command_rx) = unbounded_channel::<UICommand>();

    std::thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async_main(ui_tx, ui_command_rx)).unwrap();
    });

    ui::window::start_ui(ui_rx, args.start_minimized, ui_command_tx)
}


async fn async_main(ui_tx: tokio::sync::mpsc::UnboundedSender<BluetoothUIMessage>, mut ui_command_rx: tokio::sync::mpsc::UnboundedReceiver<UICommand>) -> bluer::Result<()> {
    let args = Args::parse();

    // let mut device_command_txs: HashMap<String, tokio::sync::mpsc::UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>> = HashMap::new();
    let mut device_managers: HashMap<String, Arc<BluetoothManager>> = HashMap::new();

    let mut managed_devices_mac: Vec<String> = Vec::new(); // includes ony non-AirPods. AirPods handled separately.

    let devices_path = get_devices_path();
    let devices_json = std::fs::read_to_string(&devices_path).unwrap_or_else(|e| {
        log::error!("Failed to read devices file: {}", e);
        "{}".to_string()
    });
    let devices_list: HashMap<String, DeviceData> = serde_json::from_str(&devices_json).unwrap_or_else(|e| {
        log::error!("Deserialization failed: {}", e);
        HashMap::new()
    });
    for (mac, device_data) in devices_list.iter() {
        match device_data.type_ {
            devices::enums::DeviceType::Nothing => {
                managed_devices_mac.push(mac.clone());
            }
            _ => {}
        }
    }

    let tray_handle = if args.no_tray {
        None
    } else {
        let tray = MyTray {
            conversation_detect_enabled: None,
            battery_l: None,
            battery_l_status: None,
            battery_r: None,
            battery_r_status: None,
            battery_c: None,
            battery_c_status: None,
            connected: false,
            listening_mode: None,
            allow_off_option: None,
            command_tx: None,
            ui_tx: Some(ui_tx.clone()),
        };
        let handle = tray.spawn().await.unwrap();
        Some(handle)
    };

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let le_tray_clone = tray_handle.clone();
    tokio::spawn(async move {
        info!("Starting LE monitor...");
        if let Err(e) = start_le_monitor(le_tray_clone).await {
            log::error!("LE monitor error: {}", e);
        }
    });

    info!("Listening for new connections.");

    info!("Checking for connected devices...");
    match find_connected_airpods(&adapter).await {
        Ok(device) => {
            let name = device.name().await?.unwrap_or_else(|| "Unknown".to_string());
            info!("Found connected AirPods: {}, initializing.", name);
            let ui_tx_clone = ui_tx.clone();
            ui_tx_clone.send(BluetoothUIMessage::DeviceConnected(device.address().to_string())).unwrap();
            let airpods_device = AirPodsDevice::new(device.address(), tray_handle.clone(), ui_tx_clone).await;
            // device_command_txs.insert(device.address().to_string(), airpods_device.command_tx.unwrap());
            // device_managers.insert(device.address().to_string(), Arc::new(airpods_device.aacp_manager));
            device_managers.insert(
                device.address().to_string(),
                Arc::from(BluetoothManager::AACP(Arc::new(airpods_device.aacp_manager))),
            );
        }
        Err(_) => {
            info!("No connected AirPods found.");
        }
    }

    match find_other_managed_devices(&adapter, managed_devices_mac.clone()).await {
        Ok(devices) => {
            for device in devices {
                let addr_str = device.address().to_string();
                info!("Found connected managed device: {}, initializing.", addr_str);
                let type_ = devices_list.get(&addr_str).unwrap().type_.clone();
                let ui_tx_clone = ui_tx.clone();
                let mut device_managers = device_managers.clone();
                tokio::spawn(async move {
                    ui_tx_clone.send(BluetoothUIMessage::DeviceConnected(addr_str.clone())).unwrap();
                    match type_ {
                        devices::enums::DeviceType::Nothing => {
                            let dev = devices::nothing::NothingDevice::new(device.address(), ui_tx_clone).await;
                            device_managers.insert(
                                addr_str,
                                Arc::from(BluetoothManager::ATT(Arc::new(dev.att_manager))),
                            );
                        }
                        _ => {}
                    }
                });
            }
        }
        Err(e) => {
            log::error!("Error finding connected managed devices: {}", e);
        }
    }

    let conn = Connection::new_system()?;
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    let device_managers_clone = device_managers.clone();
    conn.add_match(rule, move |_: (), conn, msg| {
        let Some(path) = msg.path() else { return true; };
        if !path.contains("/org/bluez/hci") || !path.contains("/dev_") {
            return true;
        }
        // debug!("PropertiesChanged signal for path: {}", path);
        let Ok((iface, changed, _)) = msg.read3::<String, HashMap<String, Variant<Box<dyn RefArg>>>, Vec<String>>() else {
            return true;
        };
        if iface != "org.bluez.Device1" {
            return true;
        }
        let Some(connected_var) = changed.get("Connected") else { return true; };
        let Some(is_connected) = connected_var.0.as_ref().as_u64() else { return true; };
        if is_connected == 0 {
            return true;
        }
        let proxy = conn.with_proxy("org.bluez", path, std::time::Duration::from_millis(5000));
        let Ok(uuids) = proxy.get::<Vec<String>>("org.bluez.Device1", "UUIDs") else { return true; };
        let target_uuid = "74ec2172-0bad-4d01-8f77-997b2be0722a";

        let Ok(addr_str) = proxy.get::<String>("org.bluez.Device1", "Address") else { return true; };
        let Ok(addr) = addr_str.parse::<Address>() else { return true; };

        if managed_devices_mac.contains(&addr_str) {
            info!("Managed device connected: {}, initializing", addr_str);
            let type_ = devices_list.get(&addr_str).unwrap().type_.clone();
            match type_ {
                devices::enums::DeviceType::Nothing => {
                    let ui_tx_clone = ui_tx.clone();
                    let mut device_managers = device_managers.clone();
                    tokio::spawn(async move {
                        ui_tx_clone.send(BluetoothUIMessage::DeviceConnected(addr_str.clone())).unwrap();
                        let dev = devices::nothing::NothingDevice::new(addr, ui_tx_clone).await;
                        device_managers.insert(
                            addr_str,
                            Arc::from(BluetoothManager::ATT(Arc::new(dev.att_manager))),
                        );
                    });
                }
                _ => {}
            }
            return true;
        }

        if !uuids.iter().any(|u| u.to_lowercase() == target_uuid) {
            return true;
        }
        let name = proxy.get::<String>("org.bluez.Device1", "Name").unwrap_or_else(|_| "Unknown".to_string());
        info!("AirPods connected: {}, initializing", name);
        let handle_clone = tray_handle.clone();
        let ui_tx_clone = ui_tx.clone();
        let mut device_managers = device_managers.clone();
        tokio::spawn(async move {
            ui_tx_clone.send(BluetoothUIMessage::DeviceConnected(addr_str.clone())).unwrap();
            let airpods_device = AirPodsDevice::new(addr, handle_clone, ui_tx_clone).await;
            device_managers.insert(
                addr_str,
                Arc::from(BluetoothManager::AACP(Arc::new(airpods_device.aacp_manager))),
            );
        });
        true
    })?;
    tokio::spawn(async move {
        while let Some(command) = ui_command_rx.recv().await {
            match command {
                UICommand::AirPods(AirPodsCommand::SetControlCommandStatus(mac, identifier, value)) => {
                    if let Some(manager) = device_managers_clone.get(&mac) {
                        match manager.as_ref() {
                            BluetoothManager::AACP(manager) => {
                                log::debug!("Sending control command to device {}: {:?} = {:?}", mac, identifier, value);
                                if let Err(e) = manager.send_control_command(identifier, value.as_ref()).await {
                                    log::error!("Failed to send control command to device {}: {}", mac, e);
                                }
                            }
                            _ => {
                                log::warn!("AACP not available for {}", mac);
                            }
                        }
                    } else {
                        log::warn!("No manager for device {}", mac);
                    }
                }
                UICommand::AirPods(AirPodsCommand::RenameDevice(mac, new_name)) => {
                    if let Some(manager) = device_managers_clone.get(&mac) {
                        match manager.as_ref() {
                            BluetoothManager::AACP(manager) => {
                                log::debug!("Renaming device {} to {}", mac, new_name);
                                if let Err(e) = manager.send_rename_packet(&new_name).await {
                                    log::error!("Failed to rename device {}: {}", mac, e);
                                }
                            }
                            _ => {
                                log::warn!("AACP not available for {}", mac);
                            }
                        }
                    } else {
                        log::warn!("No manager for device {}", mac);
                    }
                }
                UICommand::Nothing(NothingCommand::SetNoiseCancellationMode(mac, mode)) => {
                    if let Some(manager) = device_managers_clone.get(&mac) {
                        match manager.as_ref() {
                            BluetoothManager::ATT(manager) => {
                                log::debug!("Setting noise cancellation mode for device {}: {:?}", mac, mode);
                                if let Err(e) = manager.write(
                                    ATTHandles::NothingEverything,
                                    &[
                                        0x55,
                                        0x60, 0x01,
                                        0x0F, 0xF0,
                                        0x03, 0x00,
                                        0x00, 0x01, // the 0x00 is an incremental counter, but it works without it
                                        mode.to_byte(), 0x00,
                                        0x00, 0x00 // these both bytes were something random, 0 works too
                                    ]
                                ).await {
                                    log::error!("Failed to set noise cancellation mode for device {}: {}", mac, e);
                                }
                            }
                            _ => {
                                log::warn!("Nothing manager not available for {}", mac);
                            }
                        }
                    } else {
                        log::warn!("No manager for device {}", mac);
                    }
                }
            }
        }
    });

    info!("Listening for Bluetooth connections via D-Bus...");
    loop {
        conn.process(std::time::Duration::from_millis(1000))?;
    }
}