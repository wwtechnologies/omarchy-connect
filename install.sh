#!/bin/bash

# Installs the Omarchy Connect host for the current user:
#   ~/.local/bin/omarchy-connect                      host daemon and CLI
#   ~/.config/systemd/user/omarchy-connect.service    starts with the session
#   ~/.config/omarchy/plugins/omarchy-connect.status  top bar icon and panel
# and, with sudo, a udev rule for /dev/uinput and a ufw rule for the port.
#
#   ./install.sh              install or update
#   ./install.sh --no-sudo    skip the udev and firewall steps
#   ./install.sh --uninstall  remove everything above (settings are kept)

set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin="$HOME/.local/bin/omarchy-connect"
unit_dir="$HOME/.config/systemd/user"
unit="omarchy-connect.service"
plugin_id="omarchy-connect.status"
plugin_dir="$HOME/.config/omarchy/plugins/$plugin_id"
udev_rule="/etc/udev/rules.d/70-omarchy-connect-uinput.rules"
port=47921

use_sudo=1
uninstall=0
for arg in "$@"; do
  case "$arg" in
    --no-sudo) use_sudo=0 ;;
    --uninstall) uninstall=1 ;;
    -h | --help)
      sed -n '3,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "install.sh: unknown option $arg" >&2
      exit 1
      ;;
  esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

bar_has_widget() {
  jq -e --arg id "$plugin_id" '
    [.bar.layout[]?[]? | .id] | index($id) != null
  ' "$HOME/.config/omarchy/shell.json" >/dev/null 2>&1
}

if ((uninstall)); then
  step "Stopping the host"
  systemctl --user disable --now "$unit" 2>/dev/null || true
  rm -f "$unit_dir/$unit"
  systemctl --user daemon-reload
  step "Removing the bar widget"
  omarchy plugin disable "$plugin_id" 2>/dev/null || true
  rm -rf "$plugin_dir"
  rm -f "$bin"
  if ((use_sudo)) && [[ -f $udev_rule ]]; then
    step "Removing the uinput rule"
    sudo rm -f "$udev_rule"
    sudo udevadm control --reload
  fi
  echo
  echo "Omarchy Connect is removed. Settings remain in ~/.config/omarchy-connect."
  exit 0
fi

cargo=(cargo)
if ! command -v cargo >/dev/null; then
  if command -v mise >/dev/null; then
    cargo=(mise exec rust -- cargo)
  else
    echo "install.sh: Rust is required. Install it with: mise use -g rust" >&2
    exit 1
  fi
fi

step "Building the host (release)"
"${cargo[@]}" build --release --locked -p omarchy-host --target-dir "$repo/target" \
  --manifest-path "$repo/Cargo.toml"

step "Installing $bin"
install -Dm755 "$repo/target/release/omarchy-connect" "$bin"

step "Installing the bar widget"
mkdir -p "$plugin_dir"
install -m644 "$repo/omarchy/plugin/manifest.json" "$repo/omarchy/plugin/Panel.qml" "$plugin_dir/"
omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
if bar_has_widget; then
  echo "Already in the bar"
else
  omarchy plugin enable "$plugin_id" --section right --before omarchy.bluetooth 2>/dev/null ||
    omarchy plugin enable "$plugin_id" --section right ||
    echo "Could not add it to the bar. Run: omarchy plugin enable $plugin_id --section right"
fi

step "Installing the user service"
install -Dm644 "$repo/omarchy/$unit" "$unit_dir/$unit"
systemctl --user daemon-reload
systemctl --user enable "$unit" >/dev/null
systemctl --user restart "$unit"

if ((use_sudo)); then
  if ! cmp -s "$repo/omarchy/70-omarchy-connect-uinput.rules" "$udev_rule"; then
    step "Allowing remote keyboard and mouse (/dev/uinput, needs sudo)"
    sudo install -Dm644 "$repo/omarchy/70-omarchy-connect-uinput.rules" "$udev_rule"
    sudo udevadm control --reload
    sudo modprobe uinput 2>/dev/null || true
    sudo udevadm trigger --action=add --name-match=uinput 2>/dev/null ||
      sudo udevadm trigger --action=add --subsystem-match=misc
    sleep 1
    systemctl --user restart "$unit"
  fi
  if command -v ufw >/dev/null && sudo ufw status 2>/dev/null | grep -q "^Status: active"; then
    if ! sudo ufw status | grep -qE "^$port/tcp +ALLOW"; then
      step "Opening TCP $port in ufw (needs sudo)"
      sudo ufw allow "$port/tcp" comment "Omarchy Connect"
    fi
  fi
fi

step "Done"
sleep 1
"$bin" status || true
cat <<EOF

Click the Omarchy Connect icon in the top bar to set the unattended PIN.
The first connection shows the screen share picker on this machine; tick
"Allow a restore token" (or keep screencopy:allow_token_by_default in
~/.config/hypr/xdph.conf) and later sessions start without it.
EOF
