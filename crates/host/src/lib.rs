//! Omarchy Connect host.
//!
//! Listens on a LAN address and sends H.264 access units for each display to
//! a client that proves the unattended PIN. `--demo` generates a two-monitor
//! test pattern when no Hyprland session is available. Without `--demo` the
//! host captures through xdg-desktop-portal and PipeWire.

mod capture;
mod encode;
mod input;
mod lockout;
mod nal;
pub mod paths;
mod session;
pub mod settings;
pub mod state;
pub mod status;
mod tls;
mod yuv;

pub const DEFAULT_PORT: u16 = 47921;

pub use capture::{demo_displays, DEMO_HEIGHT, DEMO_WIDTH};
#[cfg(target_os = "linux")]
pub use capture::default_token_path;
pub use capture::CaptureMode;
pub use session::{run_host, HostConfig, HostReady, InputMode};
