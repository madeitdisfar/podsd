pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io

// AirPods state from podsd, through `podsctl watch` (one JSON line per change).
// Drop into a shell's services/ and read Pods.current from a bar or panel.
Singleton {
    id: root

    property bool running: false     // podsd is on the bus
    property var devices: []         // every known pair, see podsctl's doc comment
    // The pair to show: connected with its control channel up, else any connected one
    readonly property var current: devices.find(d => d.ready) ?? devices.find(d => d.connected) ?? null

    signal stemPress(string address, string press, string bud)

    function podsctl(args) {
        const device = root.current ? ["-d", root.current.address] : [];
        Quickshell.execDetached(["podsctl", ...device, ...args]);
    }
    // "off" | "noise-cancellation" | "transparency" | "adaptive"
    function setMode(mode) { podsctl(["mode", mode]); }
    // e.g. set("conversation-awareness", "on"); `podsctl settings` lists them
    function set(key, value) { podsctl(["set", key, String(value)]); }
    function connect() { podsctl(["connect"]); }
    function disconnect() { podsctl(["disconnect"]); }

    Process {
        id: watcher
        command: ["podsctl", "watch"]
        running: true
        stdout: SplitParser {
            onRead: line => {
                let msg;
                try {
                    msg = JSON.parse(line);
                } catch (e) {
                    console.warn("podsctl:", line);
                    return;
                }
                if (msg.kind === "state") {
                    root.running = msg.running;
                    root.devices = msg.devices;
                } else if (msg.kind === "stemPress") {
                    root.stemPress(msg.address, msg.press, msg.bud);
                }
            }
        }
        stderr: SplitParser {
            onRead: line => console.info("podsctl:", line)
        }
        // podsctl itself never exits while stdout is open; restart it if it does
        onExited: restart.start()
    }

    Timer {
        id: restart
        interval: 3000
        onTriggered: watcher.running = true
    }
}
