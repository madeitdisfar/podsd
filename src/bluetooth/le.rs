use bluer::monitor::{Monitor, MonitorEvent, Pattern, RssiSamplingPeriod};
use bluer::{Address, Session};
use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, BlockDecrypt};
use aes::cipher::generic_array::GenericArray;
use std::collections::{HashMap, HashSet};
use log::{info, error, debug};
use serde_json;
use crate::bluetooth::aacp::ProximityKeyType;
use futures::StreamExt;
use hex;
use std::time::Duration;
use std::path::PathBuf;

fn get_proximity_keys_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(data_dir).join("librepods").join("proximity_keys.json")
}

fn e(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let mut swapped_key = *key;
    swapped_key.reverse();
    let mut swapped_data = *data;
    swapped_data.reverse();
    let cipher = Aes128::new(&GenericArray::from(swapped_key));
    let mut block = GenericArray::from(swapped_data);
    cipher.encrypt_block(&mut block);
    let mut result: [u8; 16] = block.into();
    result.reverse();
    result
}

fn decrypt(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(&GenericArray::from(*key));
    let mut block = GenericArray::from(*data);
    cipher.decrypt_block(&mut block);
    block.into()
}

fn ah(k: &[u8; 16], r: &[u8; 3]) -> [u8; 3] {
    let mut r_padded = [0u8; 16];
    r_padded[..3].copy_from_slice(r);
    let encrypted = e(k, &r_padded);
    let mut hash = [0u8; 3];
    hash.copy_from_slice(&encrypted[..3]);
    hash
}

fn verify_rpa(addr: &str, irk: &[u8; 16]) -> bool {
    let rpa: Vec<u8> = addr.split(':')
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
    debug!("Verifying RPA: addr={}, hash={:?}, computed_hash={:?}", addr, hash, computed_hash);
    hash == computed_hash
}

pub async fn start_le_monitor() -> bluer::Result<()> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let proximity_keys: HashMap<ProximityKeyType, Vec<u8>> = std::fs::read_to_string(get_proximity_keys_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let irk = proximity_keys.get(&ProximityKeyType::Irk)
        .and_then(|v| if v.len() == 16 { Some(<[u8; 16]>::try_from(v.as_slice()).unwrap()) } else { None });
    let enc_key = proximity_keys.get(&ProximityKeyType::EncKey)
        .and_then(|v| if v.len() == 16 { Some(<[u8; 16]>::try_from(v.as_slice()).unwrap()) } else { None });
    let mut verified_macs: HashSet<Address> = HashSet::new();

    let pattern = Pattern {
        data_type: 0xFF,  // Manufacturer specific data
        start_position: 0,
        content: vec![0x4C, 0x00],  // Apple manufacturer ID (76) in LE
    };

    let mm = adapter.monitor().await?;
    let mut monitor_handle = mm
        .register(Monitor {
            monitor_type: bluer::monitor::Type::OrPatterns,
            rssi_low_threshold: None,
            rssi_high_threshold: None,
            rssi_low_timeout: None,
            rssi_high_timeout: None,
            rssi_sampling_period: Some(RssiSamplingPeriod::Period(Duration::from_millis(100))),
            patterns: Some(vec![pattern]),
            ..Default::default()
        })
        .await?;

    debug!("Started LE monitor");

    while let Some(mevt) = monitor_handle.next().await {
        if let MonitorEvent::DeviceFound(devid) = mevt {
            let dev = adapter.device(devid.device)?;
            let addr = dev.address();
            let addr_str = addr.to_string();

            debug!("Found device: {}", addr_str);

            if !verified_macs.contains(&addr) {
                debug!("Checking RPA for device: {}", addr_str);
                if let Some(irk) = &irk {
                    if verify_rpa(&addr_str, irk) {
                        verified_macs.insert(addr);
                        info!("Matched our device ({}) with the irk", addr);
                    } else {
                        debug!("Device {} did not match our irk", addr);
                    }
                }
            }

            if verified_macs.contains(&addr) {
                let mut events = dev.events().await?;
                tokio::spawn(async move {
                    while let Some(ev) = events.next().await {
                        match ev {
                            bluer::DeviceEvent::PropertyChanged(prop) => {
                                match prop {
                                    bluer::DeviceProperty::ManufacturerData(data) => {
                                        debug!("Manufacturer data from {}: {:?}", addr_str, data.iter().map(|(k, v)| (k, hex::encode(v))).collect::<HashMap<_, _>>());
                                        if let Some(enc_key) = &enc_key {
                                            if let Some(apple_data) = data.get(&76) {
                                                if apple_data.len() > 20 {
                                                    let last_16: [u8; 16] = apple_data[apple_data.len() - 16..].try_into().unwrap();
                                                    let decrypted = decrypt(enc_key, &last_16);
                                                    debug!("Decrypted data from {}: {}", addr_str, hex::encode(decrypted));
                                                    
                                                    let status = apple_data[5] as usize;
                                                    let primary_left = (status >> 5) & 0x01 == 1;
                                                    let this_in_case = (status >> 6) & 0x01 == 1;
                                                    let xor_factor = primary_left ^ this_in_case;
                                                    let is_left_in_ear = if xor_factor { (status & 0x02) != 0 } else { (status & 0x08) != 0 };
                                                    let is_right_in_ear = if xor_factor { (status & 0x08) != 0 } else { (status & 0x02) != 0 };
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
                                                    let (right_battery, right_charging) = if right_byte == 0xff {
                                                        (0, false)
                                                    } else {
                                                        (right_byte & 0x7F, (right_byte & 0x80) != 0)
                                                    };
                                                    let (case_battery, case_charging) = if case_byte == 0xff {
                                                        (0, false)
                                                    } else {
                                                        (case_byte & 0x7F, (case_byte & 0x80) != 0)
                                                    };
                                                    
                                                    info!("Battery status: Left: {}, Right: {}, Case: {}, InEar: L:{} R:{}", 
                                                          if left_byte == 0xff { "disconnected".to_string() } else { format!("{}% (charging: {})", left_battery, left_charging) },
                                                          if right_byte == 0xff { "disconnected".to_string() } else { format!("{}% (charging: {})", right_battery, right_charging) },
                                                          if case_byte == 0xff { "disconnected".to_string() } else { format!("{}% (charging: {})", case_battery, case_charging) },
                                                          is_left_in_ear, is_right_in_ear);
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
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
