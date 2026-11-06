#!/usr/bin/env bash
# Builds podsd and installs it for the current user:
#   ~/.local/bin/{podsd,podsctl}
#   ~/.config/systemd/user/podsd.service
#   ~/.local/share/dbus-1/services/io.github.madeitdisfar.Podsd.service (D-Bus activation)
# Then starts it now and at every login. `./install.sh --uninstall` removes it all
# (the saved devices file in ~/.local/share/podsd is kept).
set -euo pipefail
cd "$(dirname "$0")"

bin="$HOME/.local/bin"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
dbus="${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl --user disable --now podsd.service 2>/dev/null || true
    rm -f "$bin/podsd" "$bin/podsctl" "$units/podsd.service" "$dbus/io.github.madeitdisfar.Podsd.service"
    systemctl --user daemon-reload
    echo "podsd removed"
    exit 0
fi

cargo build --release
install -Dm755 target/release/podsd "$bin/podsd"
install -Dm755 target/release/podsctl "$bin/podsctl"
install -Dm644 dist/podsd.service "$units/podsd.service"
install -Dm644 dist/io.github.madeitdisfar.Podsd.service "$dbus/io.github.madeitdisfar.Podsd.service"
systemctl --user daemon-reload
systemctl --user enable podsd.service
systemctl --user restart podsd.service
# BlueZ's mpris-proxy tells the AirPods what's playing here. Without it they
# only ever send "play", so a double or triple press can't skip tracks.
if systemctl --user cat mpris-proxy.service >/dev/null 2>&1; then
    systemctl --user enable --now mpris-proxy.service
else
    echo "mpris-proxy (bluez-utils) not found: double / triple press won't skip tracks"
fi
echo "podsd installed and running: podsctl status"
