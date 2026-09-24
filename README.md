# Omarchy Connect

Remote desktop from a Windows client to a host on Omarchy (Arch Linux, Hyprland, Wayland). This slice stays on the LAN. The host generates an ephemeral TLS certificate and prints the SHA-256 pin of that certificate. There are no accounts and no relay.

The host encodes H.264, preferring VAAPI (`h264_vaapi` through ffmpeg) when a render node is present and falling back to libx264. The client decodes with OpenH264 and draws every display on a native egui/wgpu surface. Keyboard and mouse are sent back to the host. One file can move in each direction on the same TLS session.

`--demo` does not need a Wayland session. It sends two synthetic monitors so the protocol, decoder, and window can be exercised on a machine with no Hyprland.

## Build

```sh
cargo build
```

On Linux the host links libx264 and libpipewire-0.3. The client compiles OpenH264 from source and needs `g++` so the linker can find `libstdc++`. An X11 session also needs `libxkbcommon-x11`. VAAPI capture needs `ffmpeg` built with the `h264_vaapi` encoder. `OMARCHY_FORCE_X264=1` skips the VAAPI probe and uses libx264.

## Demo

Terminal 1:

```sh
cargo run --bin host -- --demo --bind 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin
```

Terminal 2:

```sh
cargo run --bin client -- --connect 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin
```

The window lays out each display from the session description. The demo desktop is `eDP-1` at 960×540, scale 100%, and `HDMI-A-1` at 960×540, origin x=960, scale 150%. A moving bar and the pointer (drawn into the pattern) show that frames and input are live.

Headless, once the host is listening:

```sh
cargo run --bin client -- --connect 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin --headless --frames 12
```

Files, both directions:

```sh
cargo run --bin host -- --demo --bind 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin \
  --offer-file ./notes.txt --download-dir downloads
cargo run --bin client -- --connect 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin \
  --send-file ./photo.bin --download-dir downloads
```

The client can also type a path and press Send file. Received files land in `--download-dir`.

## Host on Omarchy

```sh
cargo run --bin host -- --bind 0.0.0.0:47921 --pin-file /tmp/omarchy-connect.pin \
  --download-dir downloads --input auto
```

Without `--demo` the host opens an xdg-desktop-portal RemoteDesktop session and captures monitors through PipeWire. Hyprland needs `xdg-desktop-portal-hyprland`, and the portal dialog has to stay on a real session. `--input auto` injects the keyboard and pointer through that portal, and uses `/dev/uinput` only if portal input is not available. `--input none` captures without injecting.

The protocol already describes N displays (bounds, scale, name). This slice captures every stream the portal returns. If the portal only offers the primary monitor, the client still lays out whatever list it receives.

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

The binary is `target/x86_64-pc-windows-gnu/debug/client.exe`.

## Tests

```sh
cargo test
```

`crates/client/tests/demo_roundtrip.rs` starts a demo host, decodes frames from both monitors, and checks a file in each direction.

## What was verified

On a Linux machine with no Hyprland session and no `/dev/dri`:

- `cargo test --workspace` passed, including the demo round trip.
- `cargo run --bin host -- --demo --bind 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin --offer-file /tmp/omarchy-demo/from-host.bin --download-dir /tmp/omarchy-demo/host-in --fps 12` listened and printed a pin.
- `cargo run --bin client -- --connect 127.0.0.1:47921 --pin-file /tmp/omarchy-connect.pin --headless --frames 8 --send-file /tmp/omarchy-demo/from-client.bin --download-dir /tmp/omarchy-demo/client-in` printed two displays (`eDP-1` 960×540 scale 100%, `HDMI-A-1` 960×540 at x=960 scale 150%), 8 frames, motion on both, `sent ok`, and the two files arrived intact.
- The same client without `--headless` opened a wgpu window (llvmpipe) and stayed up. That path needs `libxkbcommon-x11` under X11.
- `cargo build -p omarchy-client --target x86_64-pc-windows-gnu` produced `target/x86_64-pc-windows-gnu/debug/client.exe`. It was not run on Windows. The MSVC target was not built here.

Portal capture and VAAPI were not executed here. Both paths are in the host: portal capture starts an xdg-desktop-portal RemoteDesktop session and reads PipeWire, and the encoder uses `h264_vaapi` when a render node and ffmpeg encoder are present.
