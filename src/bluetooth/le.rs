//! AirPods' BLE adverts ("proximity pairing", Apple type 0x07): sent often while
//! the case lid is open, rarely otherwise. They carry the lid state, which buds
//! are in the case, rough battery in the clear and exact battery encrypted with
//! the pair's key (learned over AACP). The advert's address is random, resolved
//! with the pair's IRK.
//!
//! A case with its own radio (AirPods Pro 3's) also sends a short frame of its
//! own, with its battery and that of the buds in it: while charging and for a
//! moment after the lid or a bud moves, silent when idle. Its address doesn't
//! resolve with the IRK; the pair's key tells whose case it is.
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
    /// Which bud sent it (each advertises from its own random address)
    pub source: Address,
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
    /// A bud is in the case, as far as the advertising bud knows: status bits
    /// 0x04 and 0x10, one per bud (0x02 / 0x08 for ears). Which bit is which bud
    /// swaps with the primary (0x20 / 0x40), so only "either" is reliable. Seen
    /// on AirPods Pro 3: in ears 0x2b; both in the open case 0x15 / 0x35 / 0x55 /
    /// 0x75; one taken out: the one left in 0x25 or 0x71, the one out 0x11 or 0x00.
    pub fn in_case(&self) -> bool {
        self.status & 0x14 != 0
    }

    /// What this advert says about the lid: bit 3 of the lid byte is set while
    /// it's closed (0x31 open -> 0x39 closed; the low bits count openings). A bud
    /// outside the case keeps repeating its last value (0x11 taken out, 0x12 in
    /// ears), so the hub only trusts a change in a bud's lid byte.
    pub fn lid_open(&self) -> bool {
        self.lid & 0x08 == 0
    }

    fn parse(source: Address, apple: &[u8], enc_key: Option<&[u8; 16]>, rssi: Option<i16>) -> Option<Advert> {
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
            source,
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

/// A case's own advert: `07 11 06` and one block encrypted with its pair's key,
/// which decrypts to `?? ?? ?? [case] [left] [right] ?? ?? 00 00 00 00 [seconds,
/// 4 bytes]`, each battery byte as in the buds' adverts (bit 7 charging, the rest
/// the level, `ff` absent). Read from AirPods Pro 3 (2026-11-14; the case and the
/// right bud matched the iPhone and podsd's own reports); other models' cases
/// may differ, and then nothing matches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaseAdvert {
    pub case: (i32, bool),
    /// The buds in the case; None: not in it
    pub left: Option<(i32, bool)>,
    pub right: Option<(i32, bool)>,
}

impl CaseAdvert {
    /// The frame, if it's the case of the pair with this key. Another key (or
    /// another case's frame) decrypts to random bytes, which fail the check: four
    /// zero bytes where every frame had them, and levels that are levels.
    fn parse(apple: &[u8], key: &[u8; 16]) -> Option<CaseAdvert> {
        if apple.len() != 19 || apple[..3] != [0x07, 0x11, 0x06] {
            return None;
        }
        let d = decrypt(key, apple[3..].try_into().ok()?);
        let level = |b: u8| (b != 0xff && b & 0x7f <= 100).then(|| ((b & 0x7f) as i32, b & 0x80 != 0));
        let slot_ok = |b: u8| b == 0xff || b & 0x7f <= 100;
        if d[8..12] != [0; 4] || !slot_ok(d[4]) || !slot_ok(d[5]) {
            return None;
        }
        Some(CaseAdvert { case: level(d[3])?, left: level(d[4]), right: level(d[5]) })
    }
}

/// Where an advertising address's frames come from
#[derive(Clone, Copy)]
enum Source {
    /// A bud (its address resolved with the IRK); its pair's key, once known
    Bud(Option<[u8; 16]>),
    /// The case, recognised by the key
    Case([u8; 16]),
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

/// One advert's manufacturer data, from a bud or the case of the AirPods `mac`
async fn handle(hub: &Hub, mac: &str, addr: Address, source: Source, data: &HashMap<u16, Vec<u8>>, rssi: Option<i16>) {
    let Some(apple) = data.get(&0x004C) else { return };
    match source {
        Source::Bud(key) => {
            if let Some(advert) = Advert::parse(addr, apple, key.as_ref(), rssi) {
                hub.on_advert(mac, advert).await;
            }
        }
        Source::Case(key) => {
            if let Some(case) = CaseAdvert::parse(apple, &key) {
                hub.on_case_advert(mac, case).await;
            }
        }
    }
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
    let mut cases: HashMap<Address, String> = HashMap::new();
    let mut strangers: HashSet<Address> = HashSet::new();

    while let Some(event) = monitor.next().await {
        match event {
            MonitorEvent::DeviceFound(id) => {
                let addr = id.device;
                if strangers.contains(&addr) {
                    continue;
                }
                let dev = match adapter.device(addr) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!("Advert device {}: {}", addr, e);
                        continue;
                    }
                };
                // Keys learned since the last advert count too
                let pairs = known_pairs();
                let found = match (resolved.get(&addr), cases.get(&addr)) {
                    (Some(mac), _) => {
                        let key = pairs.iter().find(|k| &k.mac == mac).and_then(|k| k.enc_key);
                        Some((mac.clone(), Source::Bud(key)))
                    }
                    (_, Some(mac)) => pairs
                        .iter()
                        .find(|k| &k.mac == mac)
                        .and_then(|k| k.enc_key)
                        .map(|key| (mac.clone(), Source::Case(key))),
                    _ => match pairs.iter().find(|k| verify_rpa(&addr, &k.irk)) {
                        Some(k) => {
                            debug!("Advert address {} is {}", addr, k.mac);
                            resolved.insert(addr, k.mac.clone());
                            Some((k.mac.clone(), Source::Bud(k.enc_key)))
                        }
                        // Not a bud of ours: maybe one of our cases, which only
                        // the key can tell
                        None => {
                            let apple = dev.manufacturer_data().await.ok().flatten().and_then(|d| d.get(&0x004C).cloned());
                            apple.and_then(|apple| {
                                pairs.iter().find_map(|k| {
                                    let key = k.enc_key?;
                                    CaseAdvert::parse(&apple, &key).map(|_| (k.mac.clone(), Source::Case(key)))
                                })
                            })
                        }
                    },
                };
                let Some((mac, source)) = found else {
                    // Other people's AirPods: forget the oldest now and then
                    if strangers.len() > 512 {
                        strangers.clear();
                    }
                    strangers.insert(addr);
                    continue;
                };
                if let Source::Case(_) = source
                    && cases.insert(addr, mac.clone()).is_none()
                {
                    debug!("Advert address {} is the case of {}", addr, mac);
                }
                let hub = hub.clone();
                tokio::spawn(async move {
                    let mut rssi = dev.rssi().await.ok().flatten();
                    if let Ok(Some(data)) = dev.manufacturer_data().await {
                        handle(&hub, &mac, addr, source, &data, rssi).await;
                    }
                    let Ok(mut events) = dev.events().await else {
                        return;
                    };
                    while let Some(bluer::DeviceEvent::PropertyChanged(prop)) = events.next().await {
                        match prop {
                            // BlueZ reports an advert only when its content changes,
                            // so with the lid open and nothing changing, the signal
                            // strength is the sign the case is still there
                            bluer::DeviceProperty::Rssi(r) => {
                                rssi = Some(r);
                                if let Source::Bud(_) = source {
                                    hub.on_advert_seen(&mac, addr).await;
                                }
                            }
                            bluer::DeviceProperty::ManufacturerData(data) => {
                                handle(&hub, &mac, addr, source, &data, rssi).await;
                            }
                            _ => {}
                        }
                    }
                });
            }
            MonitorEvent::DeviceLost(id) => {
                if let Some(mac) = resolved.get(&id.device) {
                    debug!("Adverts from {} ({}) stopped", mac, id.device);
                    hub.on_advert_lost(mac, id.device).await;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockCipherEncrypt;

    /// A made-up key: the frames below are AirPods Pro 3 case frames
    /// (2026-11-14) as they decrypted, encrypted again with it
    const KEY: [u8; 16] = *b"0123456789abcdef";

    fn frame(key: &[u8; 16], plain: [u8; 16]) -> Vec<u8> {
        let cipher = Aes128::new(&Array::from(*key));
        let mut block = Array::from(plain);
        cipher.encrypt_block(&mut block);
        let block: [u8; 16] = block.into();
        [&[0x07, 0x11, 0x06][..], &block].concat()
    }

    fn plain(state: u8, case: u8, left: u8, right: u8, secs: u16) -> [u8; 16] {
        let s = secs.to_le_bytes();
        [0x29, 0x20, state, case, left, right, 0x5b, 0x02, 0, 0, 0, 0, s[0], s[1], 0, 0]
    }

    #[test]
    fn reads_the_case_frames_seen() {
        let read = |p| CaseAdvert::parse(&frame(&KEY, p), &KEY);
        // Charging, lid closed, empty: 85% then 96% (the iPhone: 88%, then 96%)
        assert_eq!(read(plain(0x09, 0xd5, 0xff, 0xff, 1703)), Some(CaseAdvert { case: (85, true), left: None, right: None }));
        assert_eq!(read(plain(0x09, 0xe0, 0xff, 0xff, 2563)), Some(CaseAdvert { case: (96, true), left: None, right: None }));
        // Unplugged, lid opened, empty
        assert_eq!(read(plain(0x02, 0x61, 0xff, 0xff, 3566)), Some(CaseAdvert { case: (97, false), left: None, right: None }));
        // Lid closed with the right bud in, charging at 93%
        assert_eq!(
            read(plain(0x0a, 0x61, 0xff, 0xdd, 3636)),
            Some(CaseAdvert { case: (97, false), left: None, right: Some((93, true)) })
        );
        assert_eq!(read(plain(0x0b, 0x60, 0xff, 0xff, 4108)), Some(CaseAdvert { case: (96, false), left: None, right: None }));
    }

    #[test]
    fn another_pairs_case_or_other_frames_dont_match() {
        let ours = frame(&KEY, plain(0x09, 0xe0, 0xff, 0xff, 2563));
        // Another pair's key: the block decrypts to noise
        assert_eq!(CaseAdvert::parse(&ours, b"fedcba9876543210"), None);
        // Another pair's case nearby, sending under its own key
        let other = frame(b"another pair key", plain(0x0a, 0x50, 0xff, 0xff, 900));
        assert_eq!(CaseAdvert::parse(&other, &KEY), None);
        // A bud's (longer) frame, or a short one of another kind
        let mut long = ours.clone();
        long.splice(1..3, [0x19, 0x01]);
        long.extend([0; 8]);
        assert_eq!(CaseAdvert::parse(&long, &KEY), None);
        let mut kind = ours.clone();
        kind[2] = 0x01;
        assert_eq!(CaseAdvert::parse(&kind, &KEY), None);
        // Right key, but no case reading or a level out of range
        assert_eq!(CaseAdvert::parse(&frame(&KEY, plain(0x09, 0xff, 0xff, 0xff, 1)), &KEY), None);
        assert_eq!(CaseAdvert::parse(&frame(&KEY, plain(0x09, 0x61, 0x7f, 0xff, 1)), &KEY), None);
    }
}
