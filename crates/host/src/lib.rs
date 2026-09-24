//! Omarchy Connect host.
//!
//! Listens on a LAN address, serves an ephemeral TLS certificate, and sends
//! H.264 access units for each display. `--demo` generates a two-monitor test
//! pattern when no Hyprland session is available. Without `--demo` the host
//! captures through xdg-desktop-portal and PipeWire.

mod capture;
mod encode;
mod input;
mod nal;
mod session;
mod tls;
mod yuv;

pub use capture::{demo_displays, DEMO_HEIGHT, DEMO_WIDTH};
pub use session::{run_host, HostConfig, HostReady, InputMode};
