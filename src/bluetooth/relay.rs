//! The messages this computer relays through the AirPods to the other devices
//! on them (smart routing, opcode 0x10). Each is the target's address
//! (reversed), the body's length (two bytes, little-endian), and the body: 0x01
//! and one dictionary in Apple's OPACK encoding. The tags used here: 0x01 true;
//! 0x08 + n the integer n; 0x30 and 0x31 a one- and a two-byte integer;
//! 0x40 + n a string of n bytes; 0xA0 + n the n-th string or integer already
//! in the message; 0xE0 + n a dictionary of n entries.
//!
//! The contents are LibrePods' Android app's. Its ports got three bytes wrong,
//! all now written by `string`: "YES" carried a 2-byte length and btName had
//! none (the media information), and a stray 0x69 stood before idleTime (the
//! new device). The iPhone couldn't read either message, so it never learnt
//! whether this computer was playing.

use super::aacp::opcodes;

fn packet(target_mac: &str, body: &[u8]) -> Vec<u8> {
    let mut packet = vec![opcodes::SMART_ROUTING, 0x00];
    let mut target: Vec<u8> = target_mac
        .split(':')
        .map(|b| u8::from_str_radix(b, 16).unwrap_or(0))
        .collect();
    target.reverse();
    packet.extend(target);
    packet.extend((body.len() as u16).to_le_bytes());
    packet.extend_from_slice(body);
    packet
}

/// A string with its length in the tag (at most 32 bytes)
fn string(body: &mut Vec<u8>, text: &str) {
    debug_assert!(text.len() <= 32);
    body.push(0x40 + text.len() as u8);
    body.extend_from_slice(text.as_bytes());
}

fn yes_no(yes: bool) -> &'static str {
    if yes { "YES" } else { "NO" }
}

/// The greeting to a device that just connected (Android: on a new connected
/// device): whether this computer streams to the AirPods now
pub fn greeting(self_mac: &str, target_mac: &str, streaming: bool) -> Vec<u8> {
    let mut b = vec![0x01, 0xE5];
    string(&mut b, "playingApp");
    string(&mut b, "NA");
    string(&mut b, "hostStreamingState");
    string(&mut b, yes_no(streaming));
    string(&mut b, "btAddress");
    string(&mut b, self_mac);
    string(&mut b, "btName");
    string(&mut b, "Mac");
    string(&mut b, "otherDeviceAudioCategory");
    b.extend([0x30, 0x64]);
    packet(target_mac, &b)
}

/// Playback here started or stopped
pub fn media_information(self_mac: &str, target_mac: &str, streaming: bool) -> Vec<u8> {
    let mut b = vec![0x01, 0xE5];
    string(&mut b, "PlayingApp");
    string(&mut b, "com.google.ios.youtube");
    string(&mut b, "HostStreamingState");
    string(&mut b, yes_no(streaming));
    string(&mut b, "btAddress");
    string(&mut b, self_mac);
    string(&mut b, "btName");
    string(&mut b, "Mac");
    string(&mut b, "otherDeviceAudioCategory");
    b.extend([0x31, 0x2D, 0x01]);
    packet(target_mac, &b)
}

/// This computer takes the audio: the other device lets it go
pub fn hijack_request(target_mac: &str) -> Vec<u8> {
    let mut b = vec![0x01, 0xE5];
    string(&mut b, "localscore");
    b.extend([0x30, 0x64]);
    string(&mut b, "reason");
    string(&mut b, "Hijackv2");
    string(&mut b, "audioRoutingScore");
    b.extend([0x31, 0x2D, 0x01]);
    string(&mut b, "audioRoutingSetOwnershipToFalse");
    b.push(0x01);
    string(&mut b, "remotescore");
    b.push(0xA5);
    packet(target_mac, &b)
}

/// The other device shows its "moved" banner
pub fn show_nearby_ui(target_mac: &str) -> Vec<u8> {
    let mut b = vec![0x01, 0xE6];
    string(&mut b, "SmartRoutingKeyShowNearbyUI");
    b.push(0x01);
    string(&mut b, "localscore");
    b.extend([0x31, 0x2D, 0x01]);
    string(&mut b, "reason");
    string(&mut b, "hijackv2");
    string(&mut b, "audioRoutingScore");
    b.push(0xA2);
    string(&mut b, "audioRoutingSetOwnershipToFalse");
    b.push(0x01);
    string(&mut b, "remotescore");
    b.push(0xA2);
    packet(target_mac, &b)
}

/// Taking the audio back after the other device's user tapped "Move back"
pub fn hijack_reversed(target_mac: &str) -> Vec<u8> {
    let mut b = vec![0x01, 0xE3];
    string(&mut b, "audioRoutingSetOwnershipToFalse");
    b.push(0x01);
    string(&mut b, "audioRoutingShowReverseUI");
    b.push(0x01);
    string(&mut b, "reason");
    string(&mut b, "ReverseBannerTapped");
    packet(target_mac, &b)
}

/// This computer is a new device on the AirPods ("add tipi device")
pub fn new_device(self_mac: &str, target_mac: &str) -> Vec<u8> {
    let mut b = vec![0x01, 0xE5];
    string(&mut b, "idleTime");
    b.push(0x08);
    string(&mut b, "newTipi");
    b.push(0x01);
    string(&mut b, "btAddress");
    string(&mut b, self_mac);
    string(&mut b, "btName");
    string(&mut b, "Mac");
    string(&mut b, "nearbyAudioScore");
    b.push(0x0E);
    packet(target_mac, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    enum V {
        Bool(bool),
        Int(i64),
        Str(String),
        Dict(Vec<(V, V)>),
    }

    /// A strict OPACK reader for the tags above: anything else, a string
    /// running past the end, or a reference to nothing fails
    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
        /// Strings and integers so far, for 0xA0 + n
        table: Vec<V>,
    }

    impl Reader<'_> {
        fn take(&mut self, n: usize) -> Result<&[u8], String> {
            let end = self.at + n;
            let s = self.bytes.get(self.at..end).ok_or(format!("{n} bytes past the end at {}", self.at))?;
            self.at = end;
            Ok(s)
        }

        fn value(&mut self) -> Result<V, String> {
            let tag = self.take(1)?[0];
            let v = match tag {
                0x01 => return Ok(V::Bool(true)),
                0x02 => return Ok(V::Bool(false)),
                0x08..=0x2F => return Ok(V::Int(tag as i64 - 8)),
                0x30 => V::Int(self.take(1)?[0] as i8 as i64),
                0x31 => {
                    let b = self.take(2)?;
                    V::Int(i16::from_le_bytes([b[0], b[1]]) as i64)
                }
                0x40..=0x60 => {
                    let b = self.take((tag - 0x40) as usize)?.to_vec();
                    V::Str(String::from_utf8(b).map_err(|e| e.to_string())?)
                }
                0xA0..=0xBF => {
                    let n = (tag - 0xA0) as usize;
                    return self.table.get(n).cloned().ok_or(format!("reference {n} to nothing"));
                }
                0xE0..=0xEE => {
                    let mut entries = Vec::new();
                    for _ in 0..tag - 0xE0 {
                        let key = self.value()?;
                        if !matches!(key, V::Str(_)) {
                            return Err(format!("key {key:?}"));
                        }
                        entries.push((key, self.value()?));
                    }
                    return Ok(V::Dict(entries));
                }
                _ => return Err(format!("tag {tag:#04x} at {}", self.at - 1)),
            };
            self.table.push(v.clone());
            Ok(v)
        }
    }

    /// The packet's target and its dictionary, as "key=value" pairs
    fn read(packet: &[u8]) -> (Vec<u8>, Vec<String>) {
        assert_eq!(&packet[..2], &[opcodes::SMART_ROUTING, 0x00]);
        let length = u16::from_le_bytes([packet[8], packet[9]]) as usize;
        let body = &packet[10..];
        assert_eq!(length, body.len(), "declared length");
        assert_eq!(body[0], 0x01);
        let mut r = Reader { bytes: &body[1..], at: 0, table: Vec::new() };
        let V::Dict(entries) = r.value().unwrap() else { panic!("not a dictionary") };
        assert_eq!(r.at, body.len() - 1, "bytes after the dictionary");
        let text = |v: &V| match v {
            V::Bool(b) => b.to_string(),
            V::Int(i) => i.to_string(),
            V::Str(s) => s.clone(),
            V::Dict(_) => "{..}".into(),
        };
        let pairs = entries.iter().map(|(k, v)| format!("{}={}", text(k), text(v))).collect();
        (packet[2..8].to_vec(), pairs)
    }

    const ME: &str = "11:22:33:44:55:66";
    const PHONE: &str = "AA:BB:CC:DD:EE:01";

    #[test]
    fn every_message_reads_strictly() {
        let target = vec![0x01, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA];
        for streaming in [true, false] {
            let s = if streaming { "YES" } else { "NO" };
            assert_eq!(
                read(&media_information(ME, PHONE, streaming)),
                (target.clone(), vec![
                    "PlayingApp=com.google.ios.youtube".into(),
                    format!("HostStreamingState={s}"),
                    format!("btAddress={ME}"),
                    "btName=Mac".into(),
                    "otherDeviceAudioCategory=301".into(),
                ])
            );
            assert_eq!(
                read(&greeting(ME, PHONE, streaming)).1,
                vec![
                    "playingApp=NA".to_string(),
                    format!("hostStreamingState={s}"),
                    format!("btAddress={ME}"),
                    "btName=Mac".into(),
                    "otherDeviceAudioCategory=100".into(),
                ]
            );
        }
        assert_eq!(
            read(&new_device(ME, PHONE)).1,
            vec!["idleTime=0", "newTipi=true", &format!("btAddress={ME}"), "btName=Mac", "nearbyAudioScore=6"]
        );
        // The references name the routing score sent before them
        assert_eq!(
            read(&hijack_request(PHONE)).1,
            vec![
                "localscore=100",
                "reason=Hijackv2",
                "audioRoutingScore=301",
                "audioRoutingSetOwnershipToFalse=true",
                "remotescore=301",
            ]
        );
        assert_eq!(
            read(&show_nearby_ui(PHONE)).1,
            vec![
                "SmartRoutingKeyShowNearbyUI=true",
                "localscore=301",
                "reason=hijackv2",
                "audioRoutingScore=301",
                "audioRoutingSetOwnershipToFalse=true",
                "remotescore=301",
            ]
        );
        assert_eq!(
            read(&hijack_reversed(PHONE)).1,
            vec![
                "audioRoutingSetOwnershipToFalse=true",
                "audioRoutingShowReverseUI=true",
                "reason=ReverseBannerTapped",
            ]
        );
    }

    #[test]
    fn the_old_bytes_did_not_read() {
        // As sent until now: "YES" with a 2-byte length, no tag before btName
        let mut old = media_information(ME, PHONE, true);
        let yes = old.windows(4).position(|w| w == b"CYES").unwrap();
        old[yes] = 0x42;
        let name = old.windows(7).position(|w| w == b"FbtName").unwrap();
        old.remove(name);
        let mut r = Reader { bytes: &old[11..], at: 0, table: Vec::new() };
        assert!(r.value().is_err());
        // A stray 0x69 before idleTime
        let mut old = new_device(ME, PHONE);
        old.insert(12, 0x69);
        let mut r = Reader { bytes: &old[11..], at: 0, table: Vec::new() };
        assert!(r.value().is_err());
    }
}
