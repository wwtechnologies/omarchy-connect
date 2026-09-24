mod synthetic;

#[cfg(target_os = "linux")]
mod portal;

pub use synthetic::{demo_displays, SyntheticDesktop, DEMO_HEIGHT, DEMO_WIDTH};

#[cfg(target_os = "linux")]
pub use portal::{default_token_path, open_portal, PortalCapture};

use crate::input::Cursor;

#[derive(Debug)]
pub struct RawFrame {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub i420: Vec<u8>,
}

pub fn cursor_handle() -> std::sync::Arc<std::sync::Mutex<Cursor>> {
    std::sync::Arc::new(std::sync::Mutex::new(Cursor::default()))
}
