//! Omarchy Connect client.
//!
//! Connects to a host over TLS, proves the host's unattended PIN, decodes
//! H.264, and draws each display on a native egui/wgpu surface. File bytes
//! share that TLS session.

mod app;
mod decode;
mod keys;
mod net;
mod tls;

pub use app::run_gui;
pub use app::GuiLaunch;
pub use net::{
    parse_host, parse_pin, run_session, ClientCommand, FrameSlot, SessionConfig, SessionReport,
    UiEvent, UiSink, VideoFrame, DEFAULT_PORT,
};
