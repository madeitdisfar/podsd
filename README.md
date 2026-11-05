# podsd

An AirPods daemon for Linux. It talks to AirPods over Bluetooth and puts their state on the
session bus (battery, ear detection, listening mode, settings), so any shell, bar or widget
can show and control them without its own Bluetooth code.

- `podsd`: the daemon, a systemd user service
- `podsctl`: the command-line client; `podsctl watch` streams the state as JSON lines for
  shells (Quickshell, AGS, Waybar scripts)

The AirPods protocol code (AACP) comes from [LibrePods](https://github.com/librepods-org/librepods)'
Rust rewrite (`linux/rust` branch), whose history this repo keeps. podsd removes its GUI
and tray and adds the D-Bus API and the fixes listed below.

## Status

Tested with AirPods Pro 3 (A3064, firmware 90.3431000025000000.6807) on CachyOS, BlueZ 5.87,
PipeWire + WirePlumber.

| | |
|---|---|
| Battery (left, right, case, charging) | works |
| Ear detection state | works |
| Settings read (listening mode, Conversation Awareness, …) | works |
| D-Bus API, `podsctl`, `watch` stream | works |
| Changing listening mode / settings | works (checked by reading the settings back after reconnecting; the AirPods ignore mode changes while both are in the case) |
| Pause on removal, Conversation Awareness volume, takeover | LibrePods' logic, not yet re-tested here |
| BLE adverts (`bleMonitor`): battery while connected elsewhere, `CaseOpen` | works (lid and in-case bits worked out from a test with AirPods Pro 3) |

## Install

Needs Rust, BlueZ, and PipeWire with its PulseAudio server (libpulse). Pair the AirPods with
BlueZ first.

```sh
./install.sh             # build, install to ~/.local/bin, enable the user service
podsctl status
./install.sh --uninstall
```

Logs: `journalctl --user -u podsd`. `podsd --debug` also logs raw packets.

## podsctl

```
podsctl status [--json]          state of every known pair
podsctl watch                    JSON lines: the state now, then on every change
podsctl mode <off|anc|adaptive|transparency>
podsctl settings [--json]        every setting, its value and what it accepts
podsctl set <key> <value>        e.g. podsctl set conversation-awareness on
podsctl connect | disconnect
podsctl rename <name>
```

`-d <address or name>` picks the AirPods; the default is the connected pair.

### `watch` output

One object per line; each `state` line is the complete state (no deltas), so a client only
keeps the last one. A new `state` line comes on every change, and when podsd starts or stops
(`running`).

```json
{"kind":"state","running":true,"devices":[{
  "address":"AA:BB:CC:DD:EE:FF","name":"AirPods Pro","model":"A3064","firmware":"…",
  "connected":true,"ready":true,
  "battery":{"left":{"level":100,"charging":true},"right":{"level":100,"charging":true},
             "case":{"level":99,"charging":false},"headphone":{"level":null,"charging":false}},
  "batterySource":"aacp",
  "ear":{"primary":"in-case","secondary":"in-case","inEar":false},
  "listeningMode":"noise-cancellation","ownsAudio":false,
  "settings":{"conversation-awareness":false,"one-bud-anc":false,"press-speed":"default","…":"…"}}]}
{"kind":"stemPress","address":"AA:BB:CC:DD:EE:FF","press":"double","bud":"left"}
```

`level` is `null` when unknown. `connected`: BlueZ has them; `ready`: the control channel is up
(everything else is only live while it is). `ear.primary`/`secondary` are the primary and
secondary bud, not left and right: `in-ear`, `out-of-ear`, `in-case`, `disconnected` or `""`.

[`examples/quickshell/Pods.qml`](examples/quickshell/Pods.qml) is a ready Quickshell service
built on it.

## D-Bus API

Session bus, service `io.github.madeitdisfar.Podsd`.

- `/io/github/madeitdisfar/Podsd`: `org.freedesktop.DBus.ObjectManager` (lists the devices,
  announces new ones), `io.github.madeitdisfar.Podsd.Manager1` (`Version` property)
- `/io/github/madeitdisfar/Podsd/dev_XX_XX_XX_XX_XX_XX`: `io.github.madeitdisfar.Podsd.Device1`

`Device1` properties are read-only and announce changes with `PropertiesChanged`:

| Property | Type | |
|---|---|---|
| `Address`, `Name`, `Model`, `Firmware` | s | `Model` is Apple's model number, e.g. A3064 |
| `Connected` | b | BlueZ has the device connected |
| `Ready` | b | the control channel is up; the properties below are live only then |
| `BatteryLeft`, `BatteryRight`, `BatteryCase`, `BatteryHeadphone` | i | percent, -1 unknown |
| `ChargingLeft`, `ChargingRight`, `ChargingCase`, `ChargingHeadphone` | b | |
| `BatteryCaseLive`, `BatteryCaseTime` | b, x | the case reports only while a bud is in it; otherwise `BatteryCase` is the last reading (kept across restarts), taken at `BatteryCaseTime` (Unix seconds) |
| `CaseOpen` | b | the case's lid is open nearby (BLE adverts, see `bleMonitor`) |
| `BatterySource` | s | `aacp`, `ble` (from adverts, while connected elsewhere) or `""` |
| `EarPrimary`, `EarSecondary` | s | see above |
| `InEar` | b | at least one bud in an ear |
| `ListeningMode` | s | `off`, `noise-cancellation`, `adaptive`, `transparency` |
| `OwnsAudio` | b | this computer has the AirPods' audio (not the phone) |
| `Settings` | a{sv} | every setting the AirPods reported, by key |

Methods, which return once the request is sent; the new value arrives as a property change
when the AirPods apply it:

| Method | |
|---|---|
| `SetListeningMode(s mode)` | `off` needs the `allow-off-mode` setting on |
| `SetSetting(s key, v value)` | value: `b` for bool settings, `s` for choices, `i` for ints |
| `DescribeSettings() → a(sssasii)` | (key, type `bool`/`choice`/`int`, label, choices, min, max) |
| `Rename(s name)` | |
| `Connect()`, `Disconnect()` | through BlueZ |

Signal `StemPress(s press, s bud)`: press `single`/`double`/`triple`/`long`, bud
`left`/`right`. The AirPods report presses only when asked to (`stemControl` in the config).

From a shell:

```sh
busctl --user get-property io.github.madeitdisfar.Podsd \
  /io/github/madeitdisfar/Podsd/dev_AA_BB_CC_DD_EE_FF io.github.madeitdisfar.Podsd.Device1 BatteryCase
busctl --user call io.github.madeitdisfar.Podsd \
  /io/github/madeitdisfar/Podsd/dev_AA_BB_CC_DD_EE_FF io.github.madeitdisfar.Podsd.Device1 \
  SetSetting sv conversation-awareness b true
```

### Settings

| Key | Type | |
|---|---|---|
| `listening-mode` | choice | also the `ListeningMode` property |
| `allow-off-mode` | bool | offer Off among the listening modes |
| `conversation-awareness` | bool | |
| `personalized-volume` | bool | |
| `adaptive-audio-level` | int 0–100 | the raw value the AirPods report (50 by default) |
| `one-bud-anc` | bool | noise cancellation with one AirPod |
| `ear-detection` | bool | the AirPods' automatic ear detection |
| `sleep-detection` | bool | pause media when falling asleep |
| `volume-swipe` | bool | volume control by swiping the stem |
| `volume-swipe-length` | choice | `default`, `longer`, `longest` |
| `press-speed`, `press-hold-duration` | choice | `default`, `slower`, `slowest` |

The AirPods report every setting when the control channel opens but don't confirm
changes (only listening mode changes come back), so `SetSetting` updates `Settings` once
the write is sent, as LibrePods' Android app does. Two settings are never reported at all
on AirPods Pro 3, `allow-off-mode` and `ear-detection`: podsd remembers the last value
set for each pair (`~/.local/share/podsd/remembered.json`) and shows that; until one is
set, `ear-detection` is on (the factory setting) and `allow-off-mode` is missing.
Encodings follow LibrePods' Android app. Adding one
is an entry in [`src/settings.rs`](src/settings.rs); clients pick it up from
`DescribeSettings`.

## Config

`~/.config/podsd/config.json`, read at start (`systemctl --user restart podsd` after a change).
Every key is optional:

```json
{
  "earPause": true,
  "disconnectWhenNotWearing": false,
  "takeoverOnPlay": true,
  "conversationVolume": true,
  "stemControl": false,
  "restartWireplumber": false,
  "bleMonitor": true,
  "autoConnect": false,
  "caseOpenRssi": -70
}
```

| Key | |
|---|---|
| `earPause` | pause media when an AirPod comes out, resume when it goes back |
| `disconnectWhenNotWearing` | switch the AirPods' audio profile off while neither is worn |
| `takeoverOnPlay` | when media starts here with an AirPod in, take the AirPods from the phone |
| `conversationVolume` | lower the volume while Conversation Awareness hears you speak |
| `stemControl` | double press: next track, triple press: previous track |
| `restartWireplumber` | restart WirePlumber when the AirPods have no A2DP profile (cuts all audio for a moment) |
| `bleMonitor` | watch the AirPods' BLE adverts: `CaseOpen`, the case's battery, battery while connected elsewhere. Needs `Experimental = true` under `[General]` in `/etc/bluetooth/main.conf` (then `systemctl restart bluetooth`); BlueZ filters for Apple's adverts, so nothing scans actively. Without it, nothing happens |
| `autoConnect` | with `bleMonitor`: connect by itself when the lid opens and the AirPods are free (off: clients offer Connect) |
| `caseOpenRssi` | `CaseOpen` only for adverts at least this strong (dBm), so a case across the room doesn't count |

Known AirPods and their BLE keys are kept in `~/.local/share/podsd/devices.json` (mode 0600:
the keys identify the AirPods and decrypt their adverts).

## Changes from LibrePods' `linux/rust`

Found by running it against AirPods Pro 3 and reading the code:

- **The audio profile was switched off at startup.** The first ear detection report was treated
  as a removal, so with the buds in their case it paused media and set the card profile to
  `off`, which WirePlumber then remembers: the AirPods came back silent next time. The first
  report is now only a baseline, `disconnectWhenNotWearing` is off by default, and a session
  turns an `off` profile back on when it starts and when it ends.
- **The codec was downgraded when audio came back.** It picked SBC-XQ before `a2dp-sink`
  (AAC on AirPods); AAC now comes first, and the profile in use before switching off is restored.
- **Deadlock with a phone connected** (librepods PR #896 by smoothxhale): the playback listener
  held the AACP state lock across sends that take the same lock. Sending also no longer holds
  that lock while waiting on a full queue.
- **BLE keys were dropped.** The keys arrive before the device information, which wiped them
  (as described in librepods PR #877); both orders now keep them.
- **Keys in logs** (librepods PR #897 by smoothxhale): the proximity keys were logged at info
  level, and as raw packets with `--debug`. Neither is logged now.
- **Crashes on short packets:** ear detection, information and connected-devices packets were
  indexed without length checks (and `panic = "abort"` ends the process).
- **Sessions leaked across reconnects:** the AACP tasks and the playback listener ran forever
  after a disconnect; they now stop with the session.
- **WirePlumber restarts** are opt-in (`restartWireplumber`).
- BLE monitor: a failed `bluetoothctl connect` no longer blocks later attempts (as in PR #877),
  and a duplicate attempt no longer ends the advert watcher.

## License

GPL-3.0-or-later, like LibrePods (see [LICENSE](LICENSE)).
