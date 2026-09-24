use std::collections::HashMap;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::nal_units;

pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct DecoderBank {
    decoders: HashMap<u32, Decoder>,
}

impl DecoderBank {
    pub fn new() -> Self {
        Self {
            decoders: HashMap::new(),
        }
    }

    pub fn push(
        &mut self,
        display_id: u32,
        access_unit: &[u8],
    ) -> anyhow::Result<Option<RgbaFrame>> {
        let decoder = match self.decoders.entry(display_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Decoder::new().map_err(|err| anyhow::anyhow!("openh264: {err}"))?)
            }
        };
        let mut latest = None;
        for nal in nal_units(access_unit) {
            match decoder.decode(nal) {
                Ok(Some(yuv)) => {
                    let (width, height) = yuv.dimensions();
                    if width == 0 || height == 0 {
                        continue;
                    }
                    let mut rgba = vec![0u8; width * height * 4];
                    yuv.write_rgba8(&mut rgba);
                    latest = Some(RgbaFrame {
                        width: width as u32,
                        height: height as u32,
                        rgba,
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(display_id, error = %err, "decode");
                }
            }
        }
        Ok(latest)
    }
}
