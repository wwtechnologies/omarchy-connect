mod synthetic;

#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod screencopy;

pub use synthetic::{demo_displays, SyntheticDesktop, DEMO_HEIGHT, DEMO_WIDTH};

#[cfg(target_os = "linux")]
pub use portal::{default_token_path, open_portal, PortalCapture};
#[cfg(target_os = "linux")]
pub use screencopy::{open_screencopy, ScreencopyCapture};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::input::Cursor;

#[derive(Debug)]
pub struct RawFrame {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub i420: Vec<u8>,
}

/// One slot per monitor. A new capture replaces the previous one, so the
/// encoder never spends time on a picture the client will only see late.
#[derive(Clone)]
pub struct FrameInbox {
    slots: Arc<Mutex<Vec<RawFrame>>>,
    notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
}

impl FrameInbox {
    pub fn new() -> Self {
        Self {
            slots: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn publish(&self, frame: RawFrame) {
        let mut slots = self.slots.lock().expect("frame inbox");
        if let Some(slot) = slots.iter_mut().find(|item| item.display_id == frame.display_id) {
            *slot = frame;
        } else {
            slots.push(frame);
        }
        drop(slots);
        self.notify.notify_one();
    }

    pub async fn recv(&self) -> Option<RawFrame> {
        loop {
            if let Some(frame) = self.slots.lock().expect("frame inbox").pop() {
                return Some(frame);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            self.notify.notified().await;
        }
    }
}

pub fn cursor_handle() -> std::sync::Arc<std::sync::Mutex<Cursor>> {
    std::sync::Arc::new(std::sync::Mutex::new(Cursor::default()))
}

/// How a live (non-demo) session captures the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Every monitor through wlr-screencopy, else the portal.
    Auto,
    /// Every monitor through wlr-screencopy. No picker.
    Screencopy,
    /// The xdg-desktop-portal share picker.
    Portal,
}

impl CaptureMode {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "screencopy" => Ok(Self::Screencopy),
            "portal" => Ok(Self::Portal),
            other => anyhow::bail!("unknown capture mode {other} (auto, screencopy, portal)"),
        }
    }
}

#[cfg(target_os = "linux")]
pub enum LiveCapture {
    Screencopy(ScreencopyCapture),
    Portal(PortalCapture),
}

#[cfg(target_os = "linux")]
impl LiveCapture {
    pub async fn open(
        mode: CaptureMode,
        fps: u32,
        restore_token: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        if mode != CaptureMode::Portal {
            match open_screencopy(fps).await {
                Ok(capture) => return Ok(Self::Screencopy(capture)),
                Err(err) if mode == CaptureMode::Auto => {
                    tracing::warn!(error = %format!("{err:#}"), "screencopy unavailable, using the portal");
                }
                Err(err) => return Err(err),
            }
        }
        Ok(Self::Portal(open_portal(restore_token).await?))
    }

    pub fn displays(&self) -> &[omarchy_protocol::DisplayInfo] {
        match self {
            Self::Screencopy(capture) => &capture.displays,
            Self::Portal(capture) => &capture.displays,
        }
    }

    pub fn regions(&self) -> Vec<crate::input::Region> {
        match self {
            Self::Screencopy(capture) => capture.regions.clone(),
            Self::Portal(capture) => capture
                .displays
                .iter()
                .map(crate::input::Region::unscaled)
                .collect(),
        }
    }

    pub fn take_frames(&mut self) -> FrameInbox {
        match self {
            Self::Screencopy(capture) => capture.take_frames(),
            Self::Portal(capture) => capture.take_frames(),
        }
    }
}
