//! Pointer and keyboard injection.
//!
//! Demo mode draws the cursor into the test pattern and does not touch the
//! local seat unless `--input uinput` is set. A live capture injects through
//! `/dev/uinput`: xdg-desktop-portal-hyprland has no RemoteDesktop backend.
//! The installer adds a udev rule that gives the seat user access to it.

use std::collections::HashMap;

use anyhow::Context;
use omarchy_protocol::{keys, DisplayInfo, InputEvent};

/// Where a display sits in the compositor's logical layout. Video pixels
/// divide down to logical units when the monitor is scaled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Region {
    /// For captures whose display origins are already in layout units.
    pub fn unscaled(display: &DisplayInfo) -> Self {
        Self {
            id: display.id,
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
            pixel_width: display.width,
            pixel_height: display.height,
        }
    }

    fn to_layout(self, x: u32, y: u32) -> (i64, i64) {
        let lx = self.x as i64 * SUBPIXEL
            + x as i64 * self.width as i64 * SUBPIXEL / self.pixel_width.max(1) as i64;
        let ly = self.y as i64 * SUBPIXEL
            + y as i64 * self.height as i64 * SUBPIXEL / self.pixel_height.max(1) as i64;
        (lx, ly)
    }
}

/// Absolute axis steps per logical pixel, so scaled monitors keep full
/// pointer precision.
const SUBPIXEL: i64 = 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct Cursor {
    pub display_id: u32,
    pub x: u32,
    pub y: u32,
    pub visible: bool,
}

pub enum Injector {
    None,
    #[cfg(target_os = "linux")]
    Uinput(UinputDevice),
}

impl Injector {
    pub async fn apply(&mut self, event: &InputEvent) {
        match self {
            Injector::None => {}
            #[cfg(target_os = "linux")]
            Injector::Uinput(device) => {
                if let Err(err) = device.apply(event) {
                    tracing::warn!(error = %err, "uinput");
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn open_uinput(regions: &[Region]) -> anyhow::Result<UinputDevice> {
    UinputDevice::open(regions)
}

#[cfg(target_os = "linux")]
pub struct UinputDevice {
    device: evdev::uinput::VirtualDevice,
    regions: HashMap<u32, Region>,
    min_x: i64,
    min_y: i64,
    max_x: i64,
    max_y: i64,
}

#[cfg(target_os = "linux")]
impl UinputDevice {
    fn open(regions: &[Region]) -> anyhow::Result<Self> {
        use evdev::{
            uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, KeyCode,
            RelativeAxisCode, UinputAbsSetup,
        };

        let min_x = regions
            .iter()
            .map(|r| r.x as i64 * SUBPIXEL)
            .min()
            .unwrap_or(0);
        let min_y = regions
            .iter()
            .map(|r| r.y as i64 * SUBPIXEL)
            .min()
            .unwrap_or(0);
        let max_x = regions
            .iter()
            .map(|r| (r.x as i64 + r.width as i64) * SUBPIXEL - 1)
            .max()
            .unwrap_or(0)
            .max(min_x);
        let max_y = regions
            .iter()
            .map(|r| (r.y as i64 + r.height as i64) * SUBPIXEL - 1)
            .max()
            .unwrap_or(0)
            .max(min_y);
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..248 {
            keys.insert(KeyCode::new(code));
        }
        for code in [keys::BTN_LEFT, keys::BTN_RIGHT, keys::BTN_MIDDLE] {
            keys.insert(KeyCode::new(code));
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        rel.insert(RelativeAxisCode::REL_WHEEL);
        rel.insert(RelativeAxisCode::REL_HWHEEL);
        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, (max_x - min_x) as i32, 0, 0, 1),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, (max_y - min_y) as i32, 0, 0, 1),
        );
        let name = b"Omarchy Connect";
        let device = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name(name)
            .with_keys(&keys)
            .context("uinput keys")?
            .with_absolute_axis(&abs_x)
            .context("uinput abs x")?
            .with_absolute_axis(&abs_y)
            .context("uinput abs y")?
            .with_relative_axes(&rel)
            .context("uinput wheel")?
            .build()
            .context("create uinput device")?;
        let regions = regions.iter().map(|r| (r.id, *r)).collect();
        tracing::info!("uinput device ready");
        Ok(Self {
            device,
            regions,
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    fn apply(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        use evdev::{
            AbsoluteAxisCode, EventType, InputEvent as EvdevEvent, RelativeAxisCode,
            SynchronizationCode,
        };

        let mut batch = Vec::new();
        match event {
            InputEvent::MouseMove { display_id, x, y } => {
                let (lx, ly) = match self.regions.get(display_id) {
                    Some(region) => region.to_layout(*x, *y),
                    None => (self.min_x, self.min_y),
                };
                let gx = (lx.clamp(self.min_x, self.max_x) - self.min_x) as i32;
                let gy = (ly.clamp(self.min_y, self.max_y) - self.min_y) as i32;
                batch.push(EvdevEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_X.0,
                    gx,
                ));
                batch.push(EvdevEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_Y.0,
                    gy,
                ));
            }
            InputEvent::MouseButton {
                button, pressed, ..
            } => {
                if let Some(code) = keys::pointer_button(*button) {
                    batch.push(EvdevEvent::new(EventType::KEY.0, code, i32::from(*pressed)));
                }
            }
            InputEvent::MouseWheel { dx, dy, .. } => {
                if *dy != 0 {
                    batch.push(EvdevEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_WHEEL.0,
                        *dy,
                    ));
                }
                if *dx != 0 {
                    batch.push(EvdevEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_HWHEEL.0,
                        *dx,
                    ));
                }
            }
            InputEvent::Key { code, pressed } => {
                batch.push(EvdevEvent::new(
                    EventType::KEY.0,
                    *code,
                    i32::from(*pressed),
                ));
            }
        }
        if batch.is_empty() {
            return Ok(());
        }
        batch.push(EvdevEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        ));
        self.device.emit(&batch).context("emit uinput")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Region, SUBPIXEL};

    #[test]
    fn scaled_pixels_map_to_logical_layout() {
        let region = Region {
            id: 1,
            x: 1280,
            y: 0,
            width: 960,
            height: 540,
            pixel_width: 1920,
            pixel_height: 1080,
        };
        assert_eq!(region.to_layout(0, 0), (1280 * SUBPIXEL, 0));
        assert_eq!(
            region.to_layout(960, 540),
            ((1280 + 480) * SUBPIXEL, 270 * SUBPIXEL)
        );
    }
}
