# Omarchy Connect

Remote desktop from a Windows client to a host on Omarchy (Arch Linux, Hyprland, Wayland). This slice stays on the LAN. There are no accounts and no relay: the host has an unattended access PIN, set from its top bar, and the client types that PIN to connect.

The host encodes H.264, preferring VAAPI (`h264_vaapi` through ffmpeg) when a render node is present and falling back to libx264. The client decodes with OpenH264 and draws every display on a native egui/wgpu surface. Keyboard and mouse are sent back to the host. One file can move in each direction on the same TLS session.

`--demo` does not need a Wayland session. It sends two synthetic monitors so the protocol, decoder, and window can be exercised on a machine with no Hyprland.

## Install on Omarchy

```sh
./install.sh
```

This builds the host in release mode and installs:

- `~/.local/bin/omarchy-connect`, the host daemon and its CLI.
- `~/.config/systemd/user/omarchy-connect.service`, enabled so the host starts with the graphical session.
- `~/.config/omarchy/plugins/omarchy-connect.status`, a top bar icon placed before Bluetooth.
- With sudo: `/etc/udev/rules.d/70-omarchy-connect-uinput.rules`, which lets the seat user write `/dev/uinput` so the remote keyboard and mouse work, and a ufw rule for TCP 47921 when ufw is active.

`./install.sh --no-sudo` skips the udev and firewall steps. `./install.sh --uninstall` removes everything except `~/.config/omarchy-connect`. Rerun `./install.sh` after pulling to update.

After the udev rule is installed for the first time, log out and back in (or reboot) if the panel still says `/dev/uinput` is not writable.

### The bar icon

Click the icon to open the panel. It shows:

- Whether the host is running, waiting, or connected, and who is connected. **Disconnect** ends the session and the host keeps listening.
- The addresses to type on the client. Click one to copy it.
- The unattended access PIN. Saving a PIN turns unattended access on. The switch in the header turns access off without forgetting the PIN. **Remove PIN** forgets it.
- Video frame rate (15, 30, or 60) and bitrate (4, 8, or 12 Mb/s). The host uses the new values the next time a client connects.
- **Pick screens again**, only when the host fell back to the portal picker and saved a choice.

The icon is dim when access is off and highlighted while a client is connected. A desktop notification appears whenever a session starts.

### Screens

Every connection shares every monitor, with no picker. The host captures each output directly with Hyprland's wlr-screencopy protocol, reading the monitor list when the session starts, so a monitor plugged in mid-session shows up on the next connection. Scaled monitors are sent at their native resolution and the pointer is mapped through Hyprland's logical layout. Input goes through uinput, because xdg-desktop-portal-hyprland has no RemoteDesktop backend.

If the compositor has no wlr-screencopy, the host falls back to the ScreenCast portal. That shows Hyprland's share picker, which allows one monitor. Tick **Allow a restore token** and later sessions skip it. `--capture portal` forces the picker and `--capture screencopy` refuses to fall back.

### CLI

The panel is a front end for the same commands:

```sh
omarchy-connect status            # add --json for the bar
omarchy-connect pin set           # reads the PIN from stdin, or: pin set 482913
omarchy-connect pin clear
omarchy-connect unattended on|off
omarchy-connect video --fps 60 --bitrate-kbps 12000
omarchy-connect disconnect
omarchy-connect reset-share       # portal fallback only: show the picker again
journalctl --user -u omarchy-connect -f
```

With no command, `omarchy-connect` runs the host in the foreground (`--help` lists the flags). Stop the service first if you want to run it by hand: `systemctl --user stop omarchy-connect`.

## How the PIN works

The host's TLS certificate is ephemeral and the client does not pin it. After TLS, both sides run SPAKE2 (Ed25519) keyed by the PIN and exchange HMAC confirmations over a TLS exporter value, so:

- The PIN never crosses the network, even inside TLS.
- A wrong PIN fails the confirmation, and the host learns only that one guess was wrong.
- A machine in the middle cannot relay the exchange, because its two TLS legs have different exporter values. The client also rejects a host that cannot prove the PIN.

The host allows five wrong PINs, then refuses all attempts for 30 s, doubling up to 15 minutes, until the right PIN is used. PINs are 6 to 32 characters without spaces. The PIN is stored verbatim in `~/.config/omarchy-connect/settings.json` with mode 0600, because SPAKE2 needs it.

Anyone who knows the PIN can control the machine with nobody there to accept. Use a PIN you don't use elsewhere, and turn access off when you don't need it.

## Build

```sh
cargo build
```

On Linux the host links libx264 and libpipewire-0.3. The client compiles OpenH264 from source and needs `g++` so the linker can find `libstdc++`. An X11 session also needs `libxkbcommon-x11`. VAAPI capture needs `ffmpeg` built with the `h264_vaapi` encoder. `OMARCHY_FORCE_X264=1` skips the VAAPI probe and uses libx264.

## Demo

Terminal 1:

```sh
cargo run --bin omarchy-connect -- --demo --bind 127.0.0.1:47921 --pin 482913
```

Terminal 2:

```sh
cargo run --bin client -- --connect 127.0.0.1:47921 --pin 482913
```

`--pin` on the host sets a fixed PIN for that run instead of the unattended settings. The window lays out each display from the session description. The demo desktop is `eDP-1` at 960×540, scale 100%, and `HDMI-A-1` at 960×540, origin x=960, scale 150%. A moving bar and the pointer (drawn into the pattern) show that frames and input are live.

Headless, once the host is listening:

```sh
cargo run --bin client -- --connect 127.0.0.1:47921 --pin 482913 --headless --frames 12
```

Files, both directions:

```sh
cargo run --bin omarchy-connect -- --demo --bind 127.0.0.1:47921 --pin 482913 \
  --offer-file ./notes.txt --download-dir downloads
cargo run --bin client -- --connect 127.0.0.1:47921 --pin 482913 \
  --send-file ./photo.bin --download-dir downloads
```

The client can also type a path and press Send file. Received files land in `--download-dir`. The installed host puts them in `~/Downloads/Omarchy Connect`.

## Windows client

On Windows, with the MSVC toolchain:

```sh
rustup target add x86_64-pc-windows-msvc
cargo build -p omarchy-client --target x86_64-pc-windows-msvc
```

From Linux, with `mingw-w64`:

```sh
rustup target add x86_64-pc-windows-gnu
cargo build -p omarchy-client --target x86_64-pc-windows-gnu
```

The binary is `target/x86_64-pc-windows-gnu/debug/client.exe`. Enter the host address from the bar panel and the PIN. `client.exe --connect 10.0.0.5 --pin 482913` connects straight away, and `OMARCHY_CONNECT_PIN` works in place of `--pin`.

## Tests

```sh
cargo test
```

`crates/client/tests/demo_roundtrip.rs` starts a demo host, decodes frames from both monitors, and checks a file in each direction. It also checks that the host refuses clients while unattended access is off, rejects a wrong PIN, and accepts the right one. `crates/protocol/src/auth.rs` tests the PIN exchange, including a split TLS session.
