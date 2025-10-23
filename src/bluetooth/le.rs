use bluer::monitor::{Monitor, MonitorEvent, Pattern, RssiSamplingPeriod};
use bluer::{Address, Session};
use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit};
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
            rssi_sampling_period: Some(RssiSamplingPeriod::Period(Duration::from_millis(500))),
            patterns: Some(vec![pattern]),
            ..Default::default()
        })
        .await?;

    while let Some(mevt) = monitor_handle.next().await {
        if let MonitorEvent::DeviceFound(devid) = mevt {
            let dev = adapter.device(devid.device)?;
            let addr = dev.address();
            let addr_str = addr.to_string();

            if !verified_macs.contains(&addr) {
                if let Some(irk) = &irk {
                    if verify_rpa(&addr_str, irk) {
                        verified_macs.insert(addr);
                        info!("matched our device ({}) with the irk", addr);
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
                                        info!("Manufacturer data from {}: {:?}", addr_str, data.iter().map(|(k, v)| (k, hex::encode(v))).collect::<HashMap<_, _>>());
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
