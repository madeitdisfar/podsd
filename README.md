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

### Available

| Feature | Notes |
|---|---|
| Battery: left, right, case, headset, charging | The case reports only while a bud is in it; otherwise its last reading is kept, with its time |
| Ear detection state | Per bud (primary / secondary): in ear, out of ear, in case |
| Listening mode: Off, Transparency, Adaptive, Noise Cancellation | `ListeningModes` follows iOS: Off needs Off Listening Mode; with one bud in an ear, Adaptive and Noise Cancellation need Noise Cancellation with One AirPod |
| Settings | Off Listening Mode, Noise Cancellation with One AirPod, Adaptive Audio level, Conversation Awareness, Personalized Volume, Automatic Ear Detection, Pause Media When Falling Asleep, Press Speed, Press and Hold Duration, Volume Swipe and its wait. See [Settings](#settings) |
| Name | Renames the AirPods (every device sees it); the BlueZ alias follows, dropping " - Find My" |
| Connect, disconnect | Through BlueZ |
| Case opened nearby (`CaseOpen`) | From BLE adverts (`bleMonitor`, needs BlueZ's experimental mode); clients show a popup |
| Battery while connected elsewhere | From BLE adverts, decrypted with the pair's key |
| Pause / resume media when a bud comes out / goes back | `earPause`; LibrePods' logic |
| Lower the volume while you speak (Conversation Awareness) | `conversationVolume`; LibrePods' logic |
| Switching with an Apple device: shared connection, automatic takeover on playback, Move back | Needs Apple's Device ID; see [Switching between devices](#switching-between-devices) |
| Stem presses: play / pause, next, previous | The AirPods do it themselves over AVRCP, given `mpris-proxy` (see Install). `stemControl` instead has them report double / triple presses to podsd (`StemPress` signal), which skips tracks itself |
| Keep the audio codec | Turns a switched-off audio profile back on, preferring AAC |

LibrePods' logic in the last rows works but hasn't been re-tested in every case here.

### Not available

Grouped as iOS's AirPods settings group them. "Control command" means the AirPods
already report the setting over AACP (podsd sees the value), so adding it is mostly a matter
of working out its encoding.

| Feature | Notes |
|---|---|
| **Audio & Routing** | |
| Loud Sound Reduction | Control command `0x29` (reported); LibrePods notes it needs VendorID spoofing |
| Equalizer | The AirPods send EQ data (`0x53`); not parsed |
| Spatial Audio, Personalized Spatial Audio | Head tracking stream not implemented |
| Microphone (automatic / left / right) | Control command `0x01` |
| Charging Case Sounds | Control command `0x31` |
| **Controls & Gestures** | |
| Press and hold actions per bud (listening modes to cycle, Siri) | Control commands `0x16` and `0x1A` |
| Call controls (answer, mute, end) | Control command `0x24` (reported) |
| Head Gestures | Needs the head tracking stream |
| Camera Remote | iOS only |
| **Accessibility** | |
| Tone Volume | Control command `0x1F` (reported) |
| **Hearing Health** | |
| Hearing Assistance, hearing test, Hearing Aid | Control command `0x2C` (reported); LibrePods notes VendorID spoofing and ATT |
| **Battery** | |
| Optimized Charge Limit | Control command `0x3B` (reported) |
| Charging notifications | Not in podsd; a client can notify (the dots shell warns at 10 %) |
| **Others** | |
| Find My | Needs Apple's network |
| Heart rate | Control command `0x30` (reported); LibrePods has it as experimental |
| Fit test (Acoustic Seal) | Not implemented |
| High-quality two-way audio (microphone) | Work in progress in LibrePods (PRs #655, #888) |

## Install

Needs Rust, BlueZ, and PipeWire with its PulseAudio server (libpulse). Pair the AirPods with
BlueZ first.

`install.sh` also enables BlueZ's `mpris-proxy` (bluez-utils), which tells the AirPods what's
playing here over AVRCP and passes their commands to the media player. Without it the AirPods
send "play" for every stem press: double and triple press (next / previous track) do nothing.

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
podsctl mode <off|transparency|adaptive|anc>
podsctl settings [--json]        every setting, its value and what it accepts
podsctl set <key> <value>        e.g. podsctl set conversation-awareness on
podsctl connect | disconnect
podsctl rename <name>
podsctl move-here               take the AirPods over from the other device
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
| `ListeningMode` | s | `off`, `transparency`, `adaptive`, `noise-cancellation` |
| `ListeningModes` | as | the modes `SetListeningMode` takes now: Off only with `allow-off-mode`; with one bud in an ear, noise cancellation and Adaptive only with `one-bud-anc` (as on iOS) |
| `OtherDevice` | s | the other device sharing the AirPods (`iPhone`, `iPad`, `Mac`, `another device`), `""` when none |
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
| `Refresh()` | asks the AirPods to report their settings again (podsd also does this once by itself when they haven't 3 s after connecting). Right after a switch from another device, AirPods Pro 3 report nothing for about 30 s whatever is sent; `ListeningMode` is `""` meanwhile |
| `Connect()`, `Disconnect()` | through BlueZ |
| `MoveHere()` | takes the AirPods over from the other device (see [Switching between devices](#switching-between-devices)) |

Signal `MovedTo(s name, b reversed)`: another device took the AirPods; `reversed` when its
user moved them back from this computer ("Move back" on its notice). Clients show a notice
with Move back (`MoveHere`).

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
| `press-speed` | choice | `default`, `slower`, `slowest` |
| `press-hold-duration` | choice | `default`, `shorter`, `shortest` (as iOS names them) |

The AirPods report every setting when the control channel opens but don't confirm
changes (only listening mode changes come back), so `SetSetting` updates `Settings` once
the write is sent, as LibrePods' Android app does. Two settings are never reported at all
on AirPods Pro 3, `allow-off-mode` and `ear-detection`: podsd remembers the last value
set for each pair (`~/.local/share/podsd/remembered.json`) and shows that; until one is
set, `ear-detection` is on (the factory setting) and `allow-off-mode` is missing.
Encodings follow LibrePods' Android app. Adding one
is an entry in [`src/settings.rs`](src/settings.rs); clients pick it up from
`DescribeSettings`.

## Switching between devices

With Apple's Device ID on this computer (`DeviceID = bluetooth:004C:0000:0000` under
`[General]` in `/etc/bluetooth/main.conf`), the AirPods stay connected to this computer and
an Apple device (an iPhone) at once, and one of them at a time owns the audio, as between
Apple devices. Without it they drop the other device when this computer connects.

podsd follows LibrePods' Android app here:

- **Taking over**: when audio starts playing here (a real playback stream; a player that only
  reports Playing, like Spotify Connect playing on the phone, doesn't count) with an AirPod in
  an ear, podsd claims the audio, tells the other device through the AirPods (smart routing:
  media information, its "moved" UI, a hijack request), turns the audio profile on, and
  resumes playback, which the AirPods pause while switching. Automatic only with
  `takeoverOnPlay` and Apple's Device ID; `MoveHere()` does it by hand either way.
- **Losing them**: when the other device takes them, podsd pauses media here, turns the audio
  profile off (so sound doesn't go to AirPods in use elsewhere) and emits `MovedTo`. It doesn't
  take them back by itself for 3 s, and not at all after the other device's user tapped "Move
  back", until `MoveHere()`.
- podsd tells the other device whenever playback here starts or stops, and gives up the
  audio when the AirPods report the other device playing.
- Pausing (when an AirPod comes out, or another device takes them) and resuming after a
  takeover only touch players that play audio here (their process, or one it started, has an
  audio stream). A remote control such as Spotify Connect reports Playing while the phone
  plays: resuming it resumed the phone, which took the AirPods straight back.
- The phone switches silently: unlike what LibrePods saw on Android, the iPhone sends no smart
  routing messages to this computer (tested with iOS on an iPhone, AirPods Pro 3), so no
  "Moved to" notice appears on it.

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
| `earPause` | pause media when an AirPod comes out, resume when it goes back. Also off while the AirPods' `ear-detection` setting is off, as on iOS (podsd knows the value set through it; the AirPods don't report changes made elsewhere) |
| `disconnectWhenNotWearing` | switch the AirPods' audio profile off while neither is worn |
| `takeoverOnPlay` | when audio starts playing here with an AirPod in, take the AirPods over from the other device (needs Apple's Device ID; see [Switching between devices](#switching-between-devices)) |
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
