//! Two-monitor test pattern used when Hyprland is not available.
//!
//! Each display is a moving luma bar on a distinct chroma field, with the
//! monitor name, frame index, and the last pointer position drawn into the
//! Y plane. The client decodes that, so a moving bar and a cursor that follows
//! the mouse prove the session without a portal.

use std::sync::{Arc, Mutex};

use omarchy_protocol::DisplayInfo;

use crate::capture::RawFrame;
use crate::input::Cursor;

pub const DEMO_WIDTH: u32 = 960;
pub const DEMO_HEIGHT: u32 = 540;

pub fn demo_displays() -> Vec<DisplayInfo> {
    vec![
        DisplayInfo {
            id: 0,
            x: 0,
            y: 0,
            width: DEMO_WIDTH,
            height: DEMO_HEIGHT,
            scale_percent: 100,
            name: "eDP-1".into(),
        },
        DisplayInfo {
            id: 1,
            x: DEMO_WIDTH as i32,
            y: 0,
            width: DEMO_WIDTH,
            height: DEMO_HEIGHT,
            scale_percent: 150,
            name: "HDMI-A-1".into(),
        },
    ]
}

pub struct SyntheticDesktop {
    pub displays: Vec<DisplayInfo>,
    frame: u64,
    cursor: Arc<Mutex<Cursor>>,
}

impl SyntheticDesktop {
    pub fn new(cursor: Arc<Mutex<Cursor>>) -> Self {
        Self {
            displays: demo_displays(),
            frame: 0,
            cursor,
        }
    }

    pub fn render(&mut self) -> Vec<RawFrame> {
        let cursor = self.cursor.lock().ok().map(|c| *c).unwrap_or_default();
        let frames = self
            .displays
            .iter()
            .enumerate()
            .map(|(index, display)| {
                let pointer = cursor
                    .visible
                    .then_some((cursor.display_id, cursor.x, cursor.y));
                RawFrame {
                    display_id: display.id,
                    width: display.width,
                    height: display.height,
                    i420: render_i420(
                        index,
                        self.frame,
                        display.width,
                        display.height,
                        &display.name,
                        pointer
                            .filter(|(id, _, _)| *id == display.id)
                            .map(|(_, x, y)| (x, y)),
                    ),
                }
            })
            .collect();
        self.frame = self.frame.wrapping_add(1);
        frames
    }
}

pub fn render_i420(
    index: usize,
    frame: u64,
    width: u32,
    height: u32,
    label: &str,
    cursor: Option<(u32, u32)>,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut y = vec![0u8; w * h];
    let cw = w / 2;
    let ch = h / 2;
    let (cu, cv) = if index % 2 == 0 {
        (96u8, 150u8)
    } else {
        (120u8, 200u8)
    };
    let mut u = vec![cu; cw * ch];
    let mut v = vec![cv; cw * ch];
    let bar = ((frame.saturating_mul(14)) as usize) % w.max(1);
    for row in 0..h {
        for col in 0..w {
            let mut luma = 36 + ((col * 90) / w.max(1)) as u8;
            if col.abs_diff(bar) < 18 {
                luma = 230;
            }
            y[row * w + col] = luma;
        }
    }
    let text = format!("{label} {frame}");
    draw_text(&mut y, w, 24, 24, &text);
    if let Some((cx, cy)) = cursor {
        draw_cross(&mut y, w, h, cx as usize, cy as usize);
        let _ = (&mut u, &mut v);
    }
    let mut out = y;
    out.extend_from_slice(&u);
    out.extend_from_slice(&v);
    out
}

fn draw_cross(y: &mut [u8], w: usize, h: usize, cx: usize, cy: usize) {
    let arm = 14isize;
    let cx = cx as isize;
    let cy = cy as isize;
    let w = w as isize;
    let h = h as isize;
    for dx in -arm..=arm {
        let x = cx + dx;
        if x >= 0 && x < w && cy >= 0 && cy < h {
            y[(cy as usize) * (w as usize) + x as usize] = 16;
        }
    }
    for dy in -arm..=arm {
        let row = cy + dy;
        if row >= 0 && row < h && cx >= 0 && cx < w {
            y[(row as usize) * (w as usize) + cx as usize] = 16;
        }
    }
}

fn draw_text(y: &mut [u8], w: usize, origin_x: usize, origin_y: usize, text: &str) {
    let scale = 3usize;
    let mut pen = origin_x;
    for ch in text.chars() {
        let glyph = glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = pen + col * scale + sx;
                        let yy = origin_y + row * scale + sy;
                        if x < w && yy * w + x < y.len() {
                            y[yy * w + x] = 240;
                        }
                    }
                }
            }
        }
        pen += 6 * scale;
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        ' ' => [0; 7],
        _ => [
            0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_frames_differ() {
        let a = render_i420(0, 0, DEMO_WIDTH, DEMO_HEIGHT, "eDP-1", None);
        let b = render_i420(0, 4, DEMO_WIDTH, DEMO_HEIGHT, "eDP-1", None);
        assert_ne!(a, b);
        assert_eq!(a.len(), crate::yuv::i420_size(DEMO_WIDTH, DEMO_HEIGHT));
    }
}
