//! H.264 encode. VAAPI through ffmpeg when a render node is present, otherwise
//! in-process libx264 (baseline, ultrafast, zero latency) so OpenH264 can
//! decode the stream.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;

use anyhow::Context;
use x264::{Colorspace, Image, Plane, Preset, Setup, Tune};

use crate::nal::{self, AccessUnit};

pub struct EncodedFrame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

enum Backend {
    X264(X264Encoder),
    Vaapi(VaapiEncoder),
}

pub struct Encoder {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    backend: Backend,
}

// libx264 keeps its context behind raw pointers. The session task owns the
// encoder and never shares it, so moving that task between worker threads is safe.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> anyhow::Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            anyhow::bail!("encoder size must be even and non-zero, got {width}x{height}");
        }
        let backend = if force_x264() {
            Backend::X264(X264Encoder::open(width, height, fps, bitrate_kbps)?)
        } else if let Some(vaapi) = VaapiEncoder::try_open(width, height, fps) {
            tracing::info!(width, height, "encoder: h264_vaapi");
            Backend::Vaapi(vaapi)
        } else {
            tracing::info!(width, height, "encoder: software x264");
            Backend::X264(X264Encoder::open(width, height, fps, bitrate_kbps)?)
        };
        Ok(Self {
            width,
            height,
            fps,
            bitrate_kbps,
            backend,
        })
    }

    pub fn encode(&mut self, i420: &[u8], pts: u64) -> anyhow::Result<Vec<EncodedFrame>> {
        let expected = crate::yuv::i420_size(self.width, self.height);
        if i420.len() < expected {
            anyhow::bail!("i420 buffer is {} bytes, need {expected}", i420.len());
        }
        match &mut self.backend {
            Backend::X264(enc) => enc.encode(i420, pts),
            Backend::Vaapi(enc) => match enc.encode(i420) {
                Ok(frames) => Ok(frames),
                Err(err) => {
                    tracing::warn!(error = %err, "vaapi encoder failed, switching to x264");
                    let mut soft =
                        X264Encoder::open(self.width, self.height, self.fps, self.bitrate_kbps)?;
                    let frames = soft.encode(i420, pts)?;
                    self.backend = Backend::X264(soft);
                    Ok(frames)
                }
            },
        }
    }
}

struct X264Encoder {
    encoder: x264::Encoder,
    headers: Vec<u8>,
    width: i32,
    height: i32,
}

impl X264Encoder {
    fn open(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> anyhow::Result<Self> {
        let fps = fps.max(1);
        let mut encoder = Setup::preset(Preset::Ultrafast, Tune::None, true, true)
            .fps(fps, 1)
            .annexb(true)
            .bitrate(bitrate_kbps.max(200) as i32)
            .max_keyframe_interval((fps * 2) as i32)
            .min_keyframe_interval(fps as i32)
            .scenecut_threshold(0)
            .baseline()
            .build(Colorspace::I420, width as i32, height as i32)
            .map_err(|_| anyhow::anyhow!("x264 failed to open {width}x{height}"))?;
        let headers = encoder
            .headers()
            .map_err(|_| anyhow::anyhow!("x264 headers"))?
            .entirety()
            .to_vec();
        Ok(Self {
            encoder,
            headers,
            width: width as i32,
            height: height as i32,
        })
    }

    fn encode(&mut self, i420: &[u8], pts: u64) -> anyhow::Result<Vec<EncodedFrame>> {
        let y_size = (self.width * self.height) as usize;
        let c_size = ((self.width / 2) * (self.height / 2)) as usize;
        let y = &i420[..y_size];
        let u = &i420[y_size..y_size + c_size];
        let v = &i420[y_size + c_size..y_size + 2 * c_size];
        let planes = [
            Plane {
                stride: self.width,
                data: y,
            },
            Plane {
                stride: self.width / 2,
                data: u,
            },
            Plane {
                stride: self.width / 2,
                data: v,
            },
        ];
        let image = Image::new(Colorspace::I420, self.width, self.height, &planes);
        let (data, picture) = self
            .encoder
            .encode(pts as i64, image)
            .map_err(|_| anyhow::anyhow!("x264 encode"))?;
        let payload = data.entirety();
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::with_capacity(self.headers.len() + payload.len());
        if picture.keyframe() {
            bytes.extend_from_slice(&self.headers);
        }
        bytes.extend_from_slice(payload);
        Ok(vec![EncodedFrame {
            keyframe: picture.keyframe(),
            data: bytes,
        }])
    }
}

struct VaapiEncoder {
    child: Child,
    stdin: ChildStdin,
    units: mpsc::Receiver<AccessUnit>,
    stderr: std::sync::Arc<Mutex<String>>,
    _reader: Option<JoinHandle<()>>,
}

impl VaapiEncoder {
    fn try_open(width: u32, height: u32, fps: u32) -> Option<Self> {
        let device = render_node()?;
        if !ffmpeg_has_h264_vaapi() {
            return None;
        }
        match Self::spawn(&device, width, height, fps) {
            Ok(enc) => Some(enc),
            Err(err) => {
                tracing::info!(error = %err, "vaapi encoder unavailable");
                None
            }
        }
    }

    fn spawn(device: &std::path::Path, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        let gop = (fps.max(1) * 2).to_string();
        let size = format!("{width}x{height}");
        let rate = fps.max(1).to_string();
        let device = device.display().to_string();
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-vaapi_device",
                &device,
                "-fflags",
                "nobuffer",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-video_size",
                &size,
                "-framerate",
                &rate,
                "-i",
                "pipe:0",
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-bf",
                "0",
                "-g",
                &gop,
                "-qp",
                "23",
                "-f",
                "h264",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn ffmpeg h264_vaapi")?;
        let stdin = child.stdin.take().context("ffmpeg stdin")?;
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let stderr = child.stderr.take().context("ffmpeg stderr")?;
        let stderr_buf = std::sync::Arc::new(Mutex::new(String::new()));
        let stderr_log = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut err = stderr;
            let mut tmp = [0u8; 1024];
            loop {
                match err.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = stderr_log.lock() {
                            guard.push_str(&String::from_utf8_lossy(&tmp[..n]));
                            if guard.len() > 4000 {
                                let drain = guard.len() - 2000;
                                guard.drain(..drain);
                            }
                        }
                    }
                }
            }
        });
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut carry = Vec::new();
            let mut tmp = [0u8; 16 * 1024];
            loop {
                match stdout.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for unit in nal::push_bytes(&mut carry, &tmp[..n]) {
                            if tx.send(unit).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(80));
        if let Some(status) = child.try_wait().context("ffmpeg status")? {
            let err = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();
            anyhow::bail!("ffmpeg exited early ({status}): {err}");
        }
        Ok(Self {
            child,
            stdin,
            units: rx,
            stderr: stderr_buf,
            _reader: Some(reader),
        })
    }

    fn encode(&mut self, i420: &[u8]) -> anyhow::Result<Vec<EncodedFrame>> {
        self.stdin
            .write_all(i420)
            .context("write frame to ffmpeg")?;
        self.stdin.flush().context("flush ffmpeg")?;
        if let Some(status) = self.child.try_wait().context("ffmpeg status")? {
            let err = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
            anyhow::bail!("ffmpeg exited ({status}): {err}");
        }
        let mut frames = Vec::new();
        while let Ok(unit) = self.units.try_recv() {
            frames.push(EncodedFrame {
                keyframe: unit.keyframe,
                data: unit.data,
            });
        }
        Ok(frames)
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn force_x264() -> bool {
    std::env::var("OMARCHY_FORCE_X264")
        .map(|v| v != "0")
        .unwrap_or(false)
}

fn render_node() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/dev/dri").ok()?;
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

fn ffmpeg_has_h264_vaapi() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        let Ok(output) = Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains("h264_vaapi")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x264_emits_an_idr_with_start_codes() {
        std::env::set_var("OMARCHY_FORCE_X264", "1");
        let mut encoder = Encoder::open(320, 180, 15, 2000).unwrap();
        let mut i420 = vec![16u8; crate::yuv::i420_size(320, 180)];
        let y = 320 * 180;
        i420[..y].fill(180);
        i420[y..].fill(128);
        for x in 40..80 {
            for row in 20..60 {
                i420[row * 320 + x] = 16;
            }
        }
        let frames = encoder.encode(&i420, 0).unwrap();
        assert!(!frames.is_empty());
        assert!(frames[0].keyframe);
        assert!(frames[0].data.windows(4).any(|w| w == [0, 0, 0, 1]));
    }
}
