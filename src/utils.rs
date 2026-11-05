use aes::Aes128;
use aes::cipher::Array;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use crate::devices::enums::DeviceData;
use crate::settings::SettingValue;
use std::collections::HashMap;
use std::path::PathBuf;

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var(var) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(fallback),
    }
}

/// Known AirPods and their BLE keys. Holds secrets (IRK, encryption key), so it is
/// written with mode 0600.
pub fn get_devices_path() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
        .join("podsd")
        .join("devices.json")
}

/// Settings the AirPods never report (settings::UNREPORTED), as last set here:
/// { address: { key: value } }.
pub fn get_remembered_path() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
        .join("podsd")
        .join("remembered.json")
}

pub fn load_remembered() -> HashMap<String, HashMap<String, SettingValue>> {
    std::fs::read_to_string(get_remembered_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn remember_setting(address: &str, key: &str, value: &SettingValue) {
    let mut all = load_remembered();
    all.entry(address.to_string())
        .or_default()
        .insert(key.to_string(), value.clone());
    let path = get_remembered_path();
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| {
            serde_json::to_string_pretty(&all).map_err(std::io::Error::other)
        })
        .and_then(|json| std::fs::write(&path, json));
    if let Err(e) = result {
        log::error!("Failed to save {}: {}", path.display(), e);
    }
}

/// The case's last battery reading per pair: { address: [level, time] }. The case
/// only reports while a bud is in it, so this is what shows the rest of the time.
fn get_case_battery_path() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
        .join("podsd")
        .join("case-battery.json")
}

pub fn load_case_batteries() -> HashMap<String, (i32, i64)> {
    std::fs::read_to_string(get_case_battery_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_case_battery(address: &str, level: i32, time: i64) {
    let mut all = load_case_batteries();
    if all.get(address).map(|(l, _)| *l) == Some(level) {
        return;
    }
    all.insert(address.to_string(), (level, time));
    let path = get_case_battery_path();
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| serde_json::to_string(&all).map_err(std::io::Error::other))
        .and_then(|json| std::fs::write(&path, json));
    if let Err(e) = result {
        log::error!("Failed to save {}: {}", path.display(), e);
    }
}

pub fn get_config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
        .join("podsd")
        .join("config.json")
}

/// Writes the devices file atomically (temp file + rename) and owner-only.
pub fn save_devices(devices: &HashMap<String, DeviceData>) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = get_devices_path();
    let json = match serde_json::to_string_pretty(devices) {
        Ok(j) => j,
        Err(e) => {
            log::error!("Failed to serialize devices: {}", e);
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        log::error!("Failed to create {}: {}", parent.display(), e);
        return;
    }
    let tmp = path.with_extension("json.tmp");
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(json.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = result {
        log::error!("Failed to save {}: {}", path.display(), e);
    }
}

pub fn load_devices() -> HashMap<String, DeviceData> {
    std::fs::read_to_string(get_devices_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn e(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let mut swapped_key = *key;
    swapped_key.reverse();
    let mut swapped_data = *data;
    swapped_data.reverse();
    let cipher = Aes128::new(&Array::from(swapped_key));
    let mut block = Array::from(swapped_data);
    cipher.encrypt_block(&mut block);
    let mut result: [u8; 16] = block.into();
    result.reverse();
    result
}

pub fn ah(k: &[u8; 16], r: &[u8; 3]) -> [u8; 3] {
    let mut r_padded = [0u8; 16];
    r_padded[..3].copy_from_slice(r);
    let encrypted = e(k, &r_padded);
    let mut hash = [0u8; 3];
    hash.copy_from_slice(&encrypted[..3]);
    hash
}
