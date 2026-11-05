use crate::devices::enums::{DeviceData, DeviceInformation, DeviceType};
use crate::hub::Hub;
use crate::state::Battery;
use crate::utils::{ah, load_devices};
use aes::Aes128;
use aes::cipher::Array;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use bluer::monitor::{Monitor, MonitorEvent, Pattern};
use bluer::{Address, Session};
use futures::StreamExt;
use hex;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

fn decrypt(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(&Array::from(*key));
    let mut block = Array::from(*data);
    cipher.decrypt_block(&mut block);
    block.into()
}

fn verify_rpa(addr: &str, irk: &[u8; 16]) -> bool {
    let rpa: Vec<u8> = addr
        .split(':')
        .map(|s| u8::from_str_radix(s, 16).unwrap())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if rpa.len() != 6 {
        return false;
    }
    let prand_slice = &rpa[3..6];
    let prand: [u8; 3] = prand_slice.try_into().unwrap();
    let hash_slice = &rpa[0..3];
    let hash: [u8; 3] = hash_slice.try_into().unwrap();
    let computed_hash = ah(irk, &prand);
    debug!(
        "Verifying RPA: addr={}, hash={:?}, computed_hash={:?}",
        addr, hash, computed_hash
    );
    hash == computed_hash
}

/// Watches Apple BLE adverts for known AirPods: battery while they're connected
/// elsewhere, and auto-connect when they say they're free. Needs BlueZ's
/// AdvertisementMonitor (bluetoothd started with -E / --experimental).
pub async fn start_le_monitor(hub: Hub) -> bluer::Result<()> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let all_devices: HashMap<String, DeviceData> = load_devices();

    let mut verified_macs: HashMap<Address, String> = HashMap::new();
    let mut failed_macs: HashSet<Address> = HashSet::new();
    let connecting_macs = Arc::new(Mutex::new(HashSet::<Address>::new()));

    let pattern = Pattern {
        data_type: 0xFF, // Manufacturer specific data
        start_position: 0,
        content: vec![0x4C, 0x00], // Apple manufacturer ID (76) in LE
    };

    let mm = adapter.monitor().await?;
    let mut monitor_handle = mm
        .register(Monitor {
            monitor_type: bluer::monitor::Type::OrPatterns,
            rssi_low_threshold: None,
            rssi_high_threshold: None,
            rssi_low_timeout: None,
            rssi_high_timeout: None,
            rssi_sampling_period: None,
            patterns: Some(vec![pattern]),
            ..Default::default()
        })
        .await?;

    debug!("Started LE monitor");

    while let Some(mevt) = monitor_handle.next().await {
        if let MonitorEvent::DeviceFound(devid) = mevt {
            let adapter_monitor_clone = adapter.clone();
            let dev = adapter_monitor_clone.device(devid.device)?;
            let addr = dev.address();
            let addr_str = addr.to_string();

            let matched_airpods_mac: Option<String>;
            let mut matched_enc_key: Option<[u8; 16]> = None;

            if let Some(airpods_mac) = verified_macs.get(&addr) {
                matched_airpods_mac = Some(airpods_mac.clone());
            } else if failed_macs.contains(&addr) {
                continue;
            } else {
                debug!("Checking RPA for device: {}", addr_str);
                let mut found_mac = None;
                for (airpods_mac, device_data) in &all_devices {
                    if device_data.type_ == DeviceType::AirPods
                        && let Some(DeviceInformation::AirPods(info)) = &device_data.information
                        && let Ok(irk_bytes) = hex::decode(&info.le_keys.irk)
                        && irk_bytes.len() == 16
                    {
                        let irk: [u8; 16] = irk_bytes.as_slice().try_into().unwrap();
                        debug!(
                            "Verifying RPA {} for airpods MAC {}",
                            addr_str, airpods_mac
                        );
                        if verify_rpa(&addr_str, &irk) {
                            info!(
                                "Matched our device ({}) with the irk for {}",
                                addr, airpods_mac
                            );
                            verified_macs.insert(addr, airpods_mac.clone());
                            found_mac = Some(airpods_mac.clone());
                            break;
                        }
                    }
                }

                if let Some(mac) = found_mac {
                    matched_airpods_mac = Some(mac);
                } else {
                    failed_macs.insert(addr);
                    debug!("Device {} did not match any of our irks", addr);
                    continue;
                }
            }

            if let Some(ref mac) = matched_airpods_mac
                && let Some(device_data) = all_devices.get(mac)
                && let Some(DeviceInformation::AirPods(info)) = &device_data.information
                && let Ok(enc_key_bytes) = hex::decode(&info.le_keys.enc_key)
                && enc_key_bytes.len() == 16
            {
                matched_enc_key = Some(enc_key_bytes.as_slice().try_into().unwrap());
            }

            if matched_airpods_mac.is_some() {
                let mut events = dev.events().await?;
                let hub = hub.clone();
                let connecting_macs_clone = Arc::clone(&connecting_macs);
                tokio::spawn(async move {
                    while let Some(ev) = events.next().await {
                        match ev {
                            bluer::DeviceEvent::PropertyChanged(prop) => {
                                if let bluer::DeviceProperty::ManufacturerData(data) = prop {
                                    if let Some(enc_key) = &matched_enc_key
                                        && let Some(apple_data) = data.get(&76)
                                        && apple_data.len() > 20
                                    {
                                        let last_16: [u8; 16] =
                                            apple_data[apple_data.len() - 16..].try_into().unwrap();
                                        let decrypted = decrypt(enc_key, &last_16);
                                        debug!(
                                            "Decrypted data from airpods_mac {}: {}",
                                            matched_airpods_mac
                                                .as_ref()
                                                .unwrap_or(&"unknown".to_string()),
                                            hex::encode(decrypted)
                                        );

                                        let connection_state = apple_data[10] as usize;
                                        debug!("Connection state: {}", connection_state);
                                        if connection_state == 0x00 {
                                            let airpods_mac = matched_airpods_mac.clone().unwrap();
                                            if hub.config().auto_connect {
                                                let real_address = Address::from_str(&addr_str).unwrap();
                                                {
                                                    let mut cm = connecting_macs_clone.lock().await;
                                                    if !cm.insert(real_address) {
                                                        debug!("Already connecting to {}", airpods_mac);
                                                        continue;
                                                    }
                                                }
                                                info!("AirPods {} are free, connecting", airpods_mac);
                                                // bluetoothctl rather than bluer's connect, which
                                                // upstream found unreliable here.
                                                let output = tokio::process::Command::new("bluetoothctl")
                                                    .arg("connect")
                                                    .arg(&airpods_mac)
                                                    .output()
                                                    .await;
                                                match output {
                                                    Ok(o) if o.status.success() => {
                                                        info!("Connected to AirPods {}", airpods_mac)
                                                    }
                                                    Ok(o) => info!(
                                                        "Failed to connect to AirPods {}: {}",
                                                        airpods_mac,
                                                        String::from_utf8_lossy(&o.stderr)
                                                    ),
                                                    Err(e) => info!("Failed to run bluetoothctl: {}", e),
                                                }
                                                // Released on every path, so a failure doesn't
                                                // block later attempts (librepods PR #877).
                                                connecting_macs_clone.lock().await.remove(&real_address);
                                            } else {
                                                debug!("Auto-connect is off, not connecting {}", airpods_mac);
                                            }
                                        }

                                        let status = apple_data[5] as usize;
                                        let primary_left = (status >> 5) & 0x01 == 1;
                                        let this_in_case = (status >> 6) & 0x01 == 1;
                                        let xor_factor = primary_left ^ this_in_case;
                                        let is_left_in_ear = if xor_factor {
                                            (status & 0x02) != 0
                                        } else {
                                            (status & 0x08) != 0
                                        };
                                        let is_right_in_ear = if xor_factor {
                                            (status & 0x08) != 0
                                        } else {
                                            (status & 0x02) != 0
                                        };
                                        let is_flipped = !primary_left;

                                        let left_byte_index = if is_flipped { 2 } else { 1 };
                                        let right_byte_index = if is_flipped { 1 } else { 2 };

                                        let left_byte = decrypted[left_byte_index] as i32;
                                        let right_byte = decrypted[right_byte_index] as i32;
                                        let case_byte = decrypted[3] as i32;

                                        let (left_battery, left_charging) = if left_byte == 0xff {
                                            (0, false)
                                        } else {
                                            (left_byte & 0x7F, (left_byte & 0x80) != 0)
                                        };
                                        let (right_battery, right_charging) = if right_byte == 0xff
                                        {
                                            (0, false)
                                        } else {
                                            (right_byte & 0x7F, (right_byte & 0x80) != 0)
                                        };
                                        let (case_battery, case_charging) = if case_byte == 0xff {
                                            (0, false)
                                        } else {
                                            (case_byte & 0x7F, (case_byte & 0x80) != 0)
                                        };

                                        // Only while the control channel is down: when it's
                                        // up, its reports are exact and come first.
                                        if let Some(ref airpods_mac) = matched_airpods_mac
                                            && let Some(st) = hub.state(airpods_mac).await
                                            && !st.ready
                                        {
                                            let to_battery = |byte: i32, level: i32, charging: bool| {
                                                if byte == 0xff {
                                                    Battery::default()
                                                } else {
                                                    Battery { level, charging }
                                                }
                                            };
                                            let l = to_battery(left_byte, left_battery, left_charging);
                                            let r = to_battery(right_byte, right_battery, right_charging);
                                            let c = to_battery(case_byte, case_battery, case_charging);
                                            hub.update(airpods_mac, |s| {
                                                s.battery_left = l;
                                                s.battery_right = r;
                                                s.battery_case = c;
                                                s.battery_source = "ble".to_string();
                                            })
                                            .await;
                                        }

                                        debug!(
                                            "Battery status: Left: {}, Right: {}, Case: {}, InEar: L:{} R:{}",
                                            if left_byte == 0xff {
                                                "disconnected".to_string()
                                            } else {
                                                format!(
                                                    "{}% (charging: {})",
                                                    left_battery, left_charging
                                                )
                                            },
                                            if right_byte == 0xff {
                                                "disconnected".to_string()
                                            } else {
                                                format!(
                                                    "{}% (charging: {})",
                                                    right_battery, right_charging
                                                )
                                            },
                                            if case_byte == 0xff {
                                                "disconnected".to_string()
                                            } else {
                                                format!(
                                                    "{}% (charging: {})",
                                                    case_battery, case_charging
                                                )
                                            },
                                            is_left_in_ear,
                                            is_right_in_ear
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    Ok(())
}
