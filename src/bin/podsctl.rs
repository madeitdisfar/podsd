//! podsctl: command-line client for podsd.
//!
//! `podsctl watch` is meant for shells and status bars: it prints the full state as
//! one JSON object per line, first at start and again whenever anything changes
//! (including podsd starting or stopping):
//!
//!   {"kind":"state","running":true,"devices":[{"address":"…","name":"…","model":"A3064",
//!     "connected":true,"ready":true,
//!     "battery":{"left":{"level":100,"charging":true},"right":{…},"case":{…},"headphone":{…}},
//!     "batterySource":"aacp","ear":{"primary":"in-ear","secondary":"in-case","inEar":true},
//!     "listeningMode":"noise-cancellation","ownsAudio":true,"caseOpen":false,
//!     "settings":{"one-bud-anc":false,…}}]}
//!
//! battery.case also has "live" (false: the last reading, kept while no bud is
//! in the case) and "time" (when it was read, Unix seconds).
//!   {"kind":"stemPress","address":"…","press":"double","bud":"left"}
//!
//! A battery level is null when unknown. Exits when stdout closes.

#[path = "../api.rs"]
mod api;

use api::{BUS_NAME, DEVICE_IFACE, ROOT_PATH};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use serde_json::{Map, Value as Json, json};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

#[derive(Parser)]
#[command(version, about = "Control AirPods through podsd")]
struct Cli {
    /// Which AirPods: address or name. Default: the connected ones.
    #[arg(long, short = 'd', global = true)]
    device: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the AirPods' state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print the state as JSON lines, again on every change.
    Watch,
    /// Set the listening mode: off, anc (noise-cancellation), adaptive, transparency.
    Mode { mode: String },
    /// List the settings with their current values.
    Settings {
        /// As a JSON array: [{key, type, label, choices, min, max, value}]
        #[arg(long)]
        json: bool,
    },
    /// Change a setting, e.g. `podsctl set conversation-awareness on`.
    Set { key: String, value: String },
    /// Ask BlueZ to connect the AirPods.
    Connect,
    /// Ask BlueZ to disconnect the AirPods.
    Disconnect,
    /// Rename the AirPods.
    Rename { name: String },
}

type Props = HashMap<String, OwnedValue>;

fn to_json(v: &Value<'_>) -> Json {
    match v {
        Value::Bool(b) => json!(b),
        Value::Str(s) => json!(s.as_str()),
        Value::I32(i) => json!(i),
        Value::U8(i) => json!(i),
        Value::I16(i) => json!(i),
        Value::U16(i) => json!(i),
        Value::U32(i) => json!(i),
        Value::I64(i) => json!(i),
        Value::U64(i) => json!(i),
        Value::F64(f) => json!(f),
        Value::Value(inner) => to_json(inner),
        Value::Dict(d) => {
            let mut map = Map::new();
            for (k, v) in d.iter() {
                let key = match k {
                    Value::Str(s) => s.to_string(),
                    other => format!("{other}"),
                };
                map.insert(key, to_json(v));
            }
            Json::Object(map)
        }
        Value::Array(a) => Json::Array(a.iter().map(to_json).collect()),
        other => json!(format!("{other}")),
    }
}

fn get(props: &Props, key: &str) -> Json {
    props.get(key).map(|v| to_json(v)).unwrap_or(Json::Null)
}

fn battery(props: &Props, part: &str) -> Json {
    let level = get(props, &format!("Battery{part}"));
    let level = match level.as_i64() {
        Some(l) if l >= 0 => json!(l),
        _ => Json::Null,
    };
    json!({ "level": level, "charging": get(props, &format!("Charging{part}")) })
}

fn device_json(props: &Props) -> Json {
    json!({
        "address": get(props, "Address"),
        "name": get(props, "Name"),
        "model": get(props, "Model"),
        "firmware": get(props, "Firmware"),
        "connected": get(props, "Connected"),
        "ready": get(props, "Ready"),
        "battery": {
            "left": battery(props, "Left"),
            "right": battery(props, "Right"),
            "case": {
                "level": battery(props, "Case")["level"].clone(),
                "charging": get(props, "ChargingCase"),
                // false: the last reading (no bud in the case), taken at `time`
                "live": get(props, "BatteryCaseLive"),
                "time": get(props, "BatteryCaseTime"),
            },
            "headphone": battery(props, "Headphone"),
        },
        "batterySource": get(props, "BatterySource"),
        "ear": {
            "primary": get(props, "EarPrimary"),
            "secondary": get(props, "EarSecondary"),
            "inEar": get(props, "InEar"),
        },
        "listeningMode": get(props, "ListeningMode"),
        // the modes it can switch to now
        "listeningModes": get(props, "ListeningModes"),
        "ownsAudio": get(props, "OwnsAudio"),
        "caseOpen": get(props, "CaseOpen"),
        "settings": get(props, "Settings"),
    })
}

/// (object path, properties) of every device; None when podsd isn't running.
async fn devices(conn: &Connection) -> Option<Vec<(String, Props)>> {
    let proxy = ObjectManagerProxy::builder(conn)
        .destination(BUS_NAME)
        .ok()?
        .path(ROOT_PATH)
        .ok()?
        .build()
        .await
        .ok()?;
    let objects = proxy.get_managed_objects().await.ok()?;
    let mut list: Vec<(String, Props)> = objects
        .into_iter()
        .filter_map(|(path, mut ifaces)| {
            let key = ifaces.keys().find(|k| k.as_str() == DEVICE_IFACE)?.clone();
            ifaces.remove(&key).map(|props| (path.to_string(), props))
        })
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    Some(list)
}

async fn snapshot(conn: &Connection) -> Json {
    match devices(conn).await {
        Some(list) => json!({
            "kind": "state",
            "running": true,
            "devices": list.iter().map(|(_, p)| device_json(p)).collect::<Vec<_>>(),
        }),
        None => json!({ "kind": "state", "running": false, "devices": [] }),
    }
}

fn pick<'a>(list: &'a [(String, Props)], wanted: Option<&str>) -> Option<&'a (String, Props)> {
    let text = |p: &Props, k: &str| get(p, k).as_str().unwrap_or_default().to_string();
    let flag = |p: &Props, k: &str| get(p, k).as_bool().unwrap_or(false);
    match wanted {
        Some(w) => list.iter().find(|(_, p)| {
            text(p, "Address").eq_ignore_ascii_case(w) || text(p, "Name").eq_ignore_ascii_case(w)
        }),
        None => list
            .iter()
            .find(|(_, p)| flag(p, "Ready"))
            .or_else(|| list.iter().find(|(_, p)| flag(p, "Connected")))
            .or_else(|| list.first()),
    }
}

fn emit(out: &mut std::io::Stdout, value: &Json) {
    if writeln!(out, "{value}").and_then(|_| out.flush()).is_err() {
        std::process::exit(0);
    }
}

async fn watch(conn: &Connection) -> zbus::Result<()> {
    let ours = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path_namespace(ROOT_PATH)?
        .build();
    let owner = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, BUS_NAME)?
        .build();
    let mut stream = futures::stream::select(
        MessageStream::for_match_rule(ours, conn, None).await?,
        MessageStream::for_match_rule(owner, conn, None).await?,
    );

    let mut out = std::io::stdout();
    let mut last = snapshot(conn).await;
    emit(&mut out, &last);

    while let Some(msg) = stream.next().await {
        let mut pending = vec![msg];
        // Changes come in bursts (one per property); answer the burst once.
        while let Ok(Some(more)) = tokio::time::timeout(Duration::from_millis(40), stream.next()).await {
            pending.push(more);
        }
        for msg in pending.into_iter().flatten() {
            let header = msg.header();
            if header.member().map(|m| m.as_str()) == Some("StemPress")
                && let Ok((press, bud)) = msg.body().deserialize::<(String, String)>()
            {
                let path = header.path().map(|p| p.to_string()).unwrap_or_default();
                let address = path.rsplit("/dev_").next().unwrap_or_default().replace('_', ":");
                emit(
                    &mut out,
                    &json!({ "kind": "stemPress", "address": address, "press": press, "bud": bud }),
                );
            }
        }
        let now = snapshot(conn).await;
        if now != last {
            emit(&mut out, &now);
            last = now;
        }
    }
    Ok(())
}

fn fmt_battery(b: &Json) -> Option<String> {
    let level = b["level"].as_i64()?;
    Some(format!("{}%{}", level, if b["charging"].as_bool() == Some(true) { " (charging)" } else { "" }))
}

fn print_status(d: &Json) {
    let name = d["name"].as_str().unwrap_or("AirPods");
    let model = d["model"].as_str().filter(|m| !m.is_empty()).map(|m| format!(" ({m})")).unwrap_or_default();
    let state = if d["ready"].as_bool() == Some(true) {
        "connected"
    } else if d["connected"].as_bool() == Some(true) {
        "connected (no control channel)"
    } else {
        "not connected"
    };
    println!("{}{} {}: {}", name, model, d["address"].as_str().unwrap_or(""), state);
    let parts: Vec<String> = [("Left", "left"), ("Right", "right"), ("Case", "case"), ("Headset", "headphone")]
        .iter()
        .filter_map(|(label, key)| fmt_battery(&d["battery"][key]).map(|b| format!("{label} {b}")))
        .collect();
    if !parts.is_empty() {
        let source = d["batterySource"].as_str().unwrap_or("");
        println!("  Battery: {}{}", parts.join(", "), if source == "ble" { " (from BLE)" } else { "" });
    }
    let ear = &d["ear"];
    if let (Some(p), Some(s)) = (ear["primary"].as_str(), ear["secondary"].as_str())
        && !p.is_empty()
    {
        println!("  Ears: {p} / {s}");
    }
    if let Some(mode) = d["listeningMode"].as_str().filter(|m| !m.is_empty()) {
        println!("  Listening mode: {mode}");
    }
}

fn parse_value(kind: &str, choices: &[String], raw: &str) -> Result<Value<'static>, String> {
    match kind {
        "bool" => match raw.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(Value::from(true)),
            "off" | "false" | "no" | "0" => Ok(Value::from(false)),
            _ => Err(format!("expected on or off, got {raw:?}")),
        },
        "int" => raw.parse::<i32>().map(Value::from).map_err(|_| format!("expected a number, got {raw:?}")),
        _ => {
            if choices.iter().any(|c| c == raw) {
                Ok(Value::from(raw.to_string()))
            } else {
                Err(format!("expected one of {}", choices.join(", ")))
            }
        }
    }
}

type Schema = Vec<(String, String, String, Vec<String>, i32, i32)>;

async fn call<B>(conn: &Connection, path: &str, method: &str, body: &B) -> Result<zbus::Message, String>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(Some(BUS_NAME), path, Some(DEVICE_IFACE), method, body)
        .await
        .map_err(|e| match e {
            zbus::Error::MethodError(_, Some(msg), _) => msg,
            other => other.to_string(),
        })
}

async fn run(cli: Cli) -> Result<(), String> {
    let conn = Connection::session().await.map_err(|e| e.to_string())?;
    if let Command::Watch = cli.command {
        return watch(&conn).await.map_err(|e| e.to_string());
    }
    let list = devices(&conn).await.ok_or("podsd is not running")?;
    if let Command::Status { json } = cli.command {
        if json {
            println!("{}", snapshot(&conn).await);
        } else if list.is_empty() {
            println!("No AirPods known (pair them with BlueZ first)");
        } else {
            for (_, props) in &list {
                print_status(&device_json(props));
            }
        }
        return Ok(());
    }
    let (path, props) = pick(&list, cli.device.as_deref()).ok_or("No such AirPods")?;
    match cli.command {
        Command::Mode { mode } => {
            let mode = match mode.as_str() {
                "anc" | "nc" => "noise-cancellation".to_string(),
                other => other.to_string(),
            };
            call(&conn, path, "SetListeningMode", &(mode,)).await?;
        }
        Command::Settings { json } => {
            let reply = call(&conn, path, "DescribeSettings", &()).await?;
            let schema: Schema = reply.body().deserialize().map_err(|e| e.to_string())?;
            let current = get(props, "Settings");
            if json {
                let list: Vec<Json> = schema
                    .into_iter()
                    .map(|(key, kind, label, choices, min, max)| {
                        json!({
                            "key": key, "type": kind, "label": label, "choices": choices,
                            "min": min, "max": max, "value": current[&key].clone(),
                        })
                    })
                    .collect();
                println!("{}", Json::Array(list));
                return Ok(());
            }
            for (key, kind, label, choices, min, max) in schema {
                let value = match &current[&key] {
                    Json::Null => "?".to_string(),
                    Json::Bool(b) => if *b { "on" } else { "off" }.to_string(),
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let accepts = match kind.as_str() {
                    "bool" => "on|off".to_string(),
                    "int" => format!("{min}..{max}"),
                    _ => choices.join("|"),
                };
                println!("{key:<24} {value:<20} {accepts:<40} {label}");
            }
        }
        Command::Set { key, value } => {
            let reply = call(&conn, path, "DescribeSettings", &()).await?;
            let schema: Schema = reply.body().deserialize().map_err(|e| e.to_string())?;
            let (_, kind, _, choices, _, _) = schema
                .iter()
                .find(|s| s.0 == key)
                .ok_or_else(|| format!("Unknown setting {key:?} (see `podsctl settings`)"))?;
            let value = parse_value(kind, choices, &value).map_err(|e| format!("{key}: {e}"))?;
            call(&conn, path, "SetSetting", &(key, value)).await?;
        }
        Command::Connect => {
            call(&conn, path, "Connect", &()).await?;
        }
        Command::Disconnect => {
            call(&conn, path, "Disconnect", &()).await?;
        }
        Command::Rename { name } => {
            call(&conn, path, "Rename", &(name,)).await?;
        }
        Command::Status { .. } | Command::Watch => unreachable!(),
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("podsctl: {e}");
        std::process::exit(1);
    }
}
