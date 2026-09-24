//! Pointer and keyboard injection.
//!
//! Demo mode draws the cursor into the test pattern and does not touch the
//! local seat unless `--input uinput` is set. A portal capture injects through
//! `/dev/uinput`: xdg-desktop-portal-hyprland has no RemoteDesktop backend.
//! The installer adds a udev rule that gives the seat user access to it.

use std::collections::HashMap;

use anyhow::Context;
use omarchy_protocol::{keys, DisplayInfo, InputEvent};

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
pub fn open_uinput(displays: &[DisplayInfo]) -> anyhow::Result<UinputDevice> {
    UinputDevice::open(displays)
}

#[cfg(target_os = "linux")]
pub struct UinputDevice {
    device: evdev::uinput::VirtualDevice,
    origins: HashMap<u32, (i32, i32)>,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
}

#[cfg(target_os = "linux")]
impl UinputDevice {
    fn open(displays: &[DisplayInfo]) -> anyhow::Result<Self> {
        use evdev::{
            uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, KeyCode,
            RelativeAxisCode, UinputAbsSetup,
        };

        let min_x = displays.iter().map(|d| d.x).min().unwrap_or(0);
        let min_y = displays.iter().map(|d| d.y).min().unwrap_or(0);
        let max_x = displays
            .iter()
            .map(|d| d.x.saturating_add(d.width as i32).saturating_sub(1))
            .max()
            .unwrap_or(0)
            .max(min_x);
        let max_y = displays
            .iter()
            .map(|d| d.y.saturating_add(d.height as i32).saturating_sub(1))
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
            AbsInfo::new(0, 0, max_x - min_x, 0, 0, 1),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, max_y - min_y, 0, 0, 1),
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
        let origins = displays.iter().map(|d| (d.id, (d.x, d.y))).collect();
        tracing::info!("uinput device ready");
        Ok(Self {
            device,
            origins,
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
                let (ox, oy) = self
                    .origins
                    .get(display_id)
                    .copied()
                    .unwrap_or((self.min_x, self.min_y));
                let gx = ox.saturating_add(*x as i32).clamp(self.min_x, self.max_x) - self.min_x;
                let gy = oy.saturating_add(*y as i32).clamp(self.min_y, self.max_y) - self.min_y;
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
