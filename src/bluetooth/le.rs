//! AirPods' BLE adverts ("proximity pairing", Apple type 0x07): sent often while
//! the case lid is open, rarely otherwise. They carry the lid state, which buds
//! are in the case, rough battery in the clear and exact battery encrypted with
//! the pair's key (learned over AACP). The advert's address is random, resolved
//! with the pair's IRK.
//!
//! Needs BlueZ's AdvertisementMonitor (`Experimental = true` in
//! /etc/bluetooth/main.conf): BlueZ (or the controller) filters for Apple's
//! adverts, so nothing scans actively.

use crate::devices::enums::{DeviceInformation, DeviceType};
use crate::hub::Hub;
use crate::utils::{ah, load_devices};
use aes::Aes128;
use aes::cipher::Array;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use bluer::monitor::{Monitor, MonitorEvent, Pattern};
use bluer::{Address, Session};
use futures::StreamExt;
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};

fn decrypt(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(&Array::from(*key));
    let mut block = Array::from(*data);
    cipher.decrypt_block(&mut block);
    block.into()
}

/// Whether a resolvable private address was made with this IRK. Little endian,
/// as LibrePods computes it: hash = bytes 0..3, prand = bytes 3..6.
fn verify_rpa(addr: &Address, irk: &[u8; 16]) -> bool {
    let rev: Vec<u8> = addr.0.iter().rev().copied().collect();
    let prand = [rev[3], rev[4], rev[5]];
    let hash = [rev[0], rev[1], rev[2]];
    ah(irk, &prand) == hash
}

/// One proximity advert, decoded.
#[derive(Debug, Clone)]
pub struct Advert {
    /// Byte 8: lid state (see `lid_open`)
    pub lid: u8,
    pub status: u8,
    /// Byte 10: 0x00 while the AirPods aren't connected to any device
    pub connection: u8,
    /// (level, charging) from the decrypted part; None when that part is absent
    pub left: Option<(i32, bool)>,
    pub right: Option<(i32, bool)>,
    pub case: Option<(i32, bool)>,
    pub rssi: Option<i16>,
}

impl Advert {
    /// PROVISIONAL, to be checked against a real case: bit 3 of the lid byte
    /// set while the lid is closed. The buds learn the lid's state from the case,
    /// so it only means something while the advertising bud is in it (status bit
    /// 6); with both buds in ears the byte read "open" (0x12).
    pub fn lid_open(&self) -> bool {
        self.status & 0x40 != 0 && self.lid & 0x08 == 0
    }

    fn parse(apple: &[u8], enc_key: Option<&[u8; 16]>, rssi: Option<i16>) -> Option<Advert> {
        // [0] type 0x07, [1] length, [2] prefix, [3..5] model, [5] status,
        // [6] buds battery (rough), [7] charging flags + case battery (rough),
        // [8] lid, [9] color, [10] connection state, [11..27] encrypted
        if apple.len() < 27 || apple[0] != 0x07 {
            return None;
        }
        let status = apple[5];
        let (mut left, mut right, mut case) = (None, None, None);
        if let Some(key) = enc_key {
            let block: [u8; 16] = apple[apple.len() - 16..].try_into().ok()?;
            let d = decrypt(key, &block);
            // Which byte is which bud depends on which one is primary
            let primary_left = (status >> 5) & 0x01 == 1;
            let (li, ri) = if primary_left { (1, 2) } else { (2, 1) };
            let part = |byte: u8| (byte != 0xff).then(|| ((byte & 0x7f) as i32, byte & 0x80 != 0));
            left = part(d[li]);
            right = part(d[ri]);
            case = part(d[3]);
        }
        Some(Advert {
            lid: apple[8],
            status,
            connection: apple[10],
            left,
            right,
            case,
            rssi,
        })
    }
}

struct Known {
    mac: String,
    irk: [u8; 16],
    enc_key: Option<[u8; 16]>,
}

fn known_pairs() -> Vec<Known> {
    load_devices()
        .into_iter()
        .filter_map(|(mac, data)| {
            let Some(DeviceInformation::AirPods(info)) = data.information else {
                return None;
            };
            if data.type_ != DeviceType::AirPods {
                return None;
            }
            let irk: [u8; 16] = hex::decode(&info.le_keys.irk).ok()?.try_into().ok()?;
            let enc_key = hex::decode(&info.le_keys.enc_key)
                .ok()
                .and_then(|k| k.try_into().ok());
            Some(Known { mac, irk, enc_key })
        })
        .collect()
}

pub async fn start_le_monitor(hub: Hub) -> bluer::Result<()> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;

    let mm = adapter.monitor().await?;
    let mut monitor = mm
        .register(Monitor {
            monitor_type: bluer::monitor::Type::OrPatterns,
            patterns: Some(vec![Pattern {
                data_type: 0xFF, // manufacturer data: Apple (0x004C), proximity pairing (0x07)
                start_position: 0,
                content: vec![0x4C, 0x00, 0x07],
            }]),
            ..Default::default()
        })
        .await?;
    info!("Watching for AirPods adverts");

    let mut resolved: HashMap<Address, String> = HashMap::new();
    let mut strangers: HashSet<Address> = HashSet::new();

    while let Some(event) = monitor.next().await {
        match event {
            MonitorEvent::DeviceFound(id) => {
                let addr = id.device;
                if strangers.contains(&addr) {
                    continue;
                }
                // Keys learned since the last advert count too
                let pairs = known_pairs();
                let mac = match resolved.get(&addr) {
                    Some(mac) => mac.clone(),
                    None => match pairs.iter().find(|k| verify_rpa(&addr, &k.irk)) {
                        Some(k) => {
                            debug!("Advert address {} is {}", addr, k.mac);
                            resolved.insert(addr, k.mac.clone());
                            k.mac.clone()
                        }
                        None => {
                            // Other people's AirPods: forget the oldest now and then
                            if strangers.len() > 512 {
                                strangers.clear();
                            }
                            strangers.insert(addr);
                            continue;
                        }
                    },
                };
                let enc_key = pairs.iter().find(|k| k.mac == mac).and_then(|k| k.enc_key);
                let dev = match adapter.device(addr) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!("Advert device {}: {}", addr, e);
                        continue;
                    }
                };
                let hub = hub.clone();
                tokio::spawn(async move {
                    let mut rssi = dev.rssi().await.ok().flatten();
                    let handle = |data: &HashMap<u16, Vec<u8>>, rssi: Option<i16>| {
                        data.get(&0x004C)
                            .and_then(|apple| Advert::parse(apple, enc_key.as_ref(), rssi))
                    };
                    if let Ok(Some(data)) = dev.manufacturer_data().await
                        && let Some(advert) = handle(&data, rssi)
                    {
                        hub.on_advert(&mac, advert).await;
                    }
                    let Ok(mut events) = dev.events().await else {
                        return;
                    };
                    while let Some(bluer::DeviceEvent::PropertyChanged(prop)) = events.next().await {
                        match prop {
                            bluer::DeviceProperty::Rssi(r) => rssi = Some(r),
                            bluer::DeviceProperty::ManufacturerData(data) => {
                                if let Some(advert) = handle(&data, rssi) {
                                    hub.on_advert(&mac, advert).await;
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
            MonitorEvent::DeviceLost(id) => {
                if let Some(mac) = resolved.get(&id.device) {
                    debug!("Adverts from {} stopped", mac);
                    hub.on_advert_lost(mac).await;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
