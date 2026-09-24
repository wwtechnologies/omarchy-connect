//! xdg-desktop-portal ScreenCast + RemoteDesktop, then PipeWire.
//!
//! Hyprland has no X11 root window. The portal (xdg-desktop-portal-hyprland)
//! is the capture and input path: RemoteDesktop creates the session, ScreenCast
//! selects monitors, and PipeWire delivers raw frames. Pointer and key events
//! go back through RemoteDesktop so the compositor injects them.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType, Stream as PortalStream};
use ashpd::desktop::PersistMode;
use omarchy_protocol::{keys, DisplayInfo, InputEvent};
use pipewire as pw;
use pw::spa;

use crate::capture::RawFrame;
use crate::yuv::{self, PixelOrder};

pub struct PortalCapture {
    pub displays: Vec<DisplayInfo>,
    frames: Option<tokio::sync::mpsc::Receiver<RawFrame>>,
    input: Option<PortalInput>,
    stop: Arc<AtomicBool>,
}

pub struct PortalInput {
    tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl PortalCapture {
    pub fn take_frames(&mut self) -> tokio::sync::mpsc::Receiver<RawFrame> {
        self.frames.take().expect("portal frames taken once")
    }

    pub fn take_input(&mut self) -> Option<PortalInput> {
        self.input.take()
    }
}

impl PortalInput {
    pub async fn apply(&self, event: InputEvent) {
        let _ = self.tx.send(event);
    }
}

pub async fn open_portal() -> anyhow::Result<PortalCapture> {
    let remote = RemoteDesktop::new()
        .await
        .context("connect to org.freedesktop.portal.RemoteDesktop")?;
    let screencast = Screencast::new()
        .await
        .context("connect to org.freedesktop.portal.ScreenCast")?;
    let session = remote
        .create_session()
        .await
        .context("create remote-desktop session")?;
    remote
        .select_devices(
            &session,
            DeviceType::Keyboard | DeviceType::Pointer,
            None,
            PersistMode::DoNot,
        )
        .await
        .context("select keyboard and pointer")?
        .response()
        .context("remote-desktop device response")?;
    screencast
        .select_sources(
            &session,
            CursorMode::Embedded,
            SourceType::Monitor | SourceType::Window,
            true,
            None,
            PersistMode::DoNot,
        )
        .await
        .context("select screencast sources")?
        .response()
        .context("screencast source response")?;
    let started = remote
        .start(&session, None)
        .await
        .context("start remote-desktop session. A portal dialog should be visible; select the monitors to share")?;
    let selected = started.response().context("portal start was cancelled")?;
    let streams = selected
        .streams()
        .context("portal started without screencast streams")?
        .to_vec();
    if streams.is_empty() {
        anyhow::bail!("portal returned no streams");
    }
    let fd = screencast
        .open_pipe_wire_remote(&session)
        .await
        .context("open pipewire remote")?;

    let (displays, targets) = displays_from_streams(&streams);
    let nodes: HashMap<u32, u32> = targets.iter().map(|t| (t.display_id, t.node_id)).collect();
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(2);
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    std::thread::Builder::new()
        .name("omarchy-pipewire".into())
        .spawn(move || {
            if let Err(err) = pipewire_thread(fd, targets, frame_tx, stop_thread) {
                tracing::error!(error = %err, "pipewire capture ended");
            }
        })
        .context("spawn pipewire thread")?;

    tokio::spawn(async move {
        let _keep_screencast = screencast;
        while let Some(event) = input_rx.recv().await {
            if let Err(err) = apply_portal_input(&remote, &session, &nodes, &event).await {
                tracing::warn!(error = %err, "portal input");
            }
        }
    });

    tracing::info!(count = displays.len(), "portal capture started");
    Ok(PortalCapture {
        displays,
        frames: Some(frame_rx),
        input: Some(PortalInput { tx: input_tx }),
        stop,
    })
}

struct StreamTarget {
    display_id: u32,
    node_id: u32,
}

fn displays_from_streams(streams: &[PortalStream]) -> (Vec<DisplayInfo>, Vec<StreamTarget>) {
    let mut displays = Vec::new();
    let mut targets = Vec::new();
    let mut cursor_x = 0i32;
    for (index, stream) in streams.iter().enumerate() {
        let (lw, lh) = stream.size().unwrap_or((1280, 720));
        let width = yuv::even(lw.max(2) as u32).max(2);
        let height = yuv::even(lh.max(2) as u32).max(2);
        let (x, y) = stream.position().unwrap_or((cursor_x, 0));
        let name = stream
            .id()
            .filter(|id| !id.is_empty())
            .unwrap_or("monitor")
            .to_string();
        displays.push(DisplayInfo {
            id: index as u32,
            x,
            y,
            width,
            height,
            scale_percent: 100,
            name,
        });
        targets.push(StreamTarget {
            display_id: index as u32,
            node_id: stream.pipe_wire_node_id(),
        });
        cursor_x = x.saturating_add(width as i32);
    }
    (displays, targets)
}

async fn apply_portal_input(
    remote: &RemoteDesktop<'_>,
    session: &ashpd::desktop::Session<'_, RemoteDesktop<'_>>,
    nodes: &HashMap<u32, u32>,
    event: &InputEvent,
) -> anyhow::Result<()> {
    match event {
        InputEvent::MouseMove { display_id, x, y } => {
            let node = nodes.get(display_id).copied().unwrap_or(0);
            remote
                .notify_pointer_motion_absolute(session, node, f64::from(*x), f64::from(*y))
                .await?;
        }
        InputEvent::MouseButton {
            button, pressed, ..
        } => {
            let Some(code) = keys::pointer_button(*button) else {
                return Ok(());
            };
            let state = if *pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            remote
                .notify_pointer_button(session, i32::from(code), state)
                .await?;
        }
        InputEvent::MouseWheel { dx, dy, .. } => {
            if *dy != 0 {
                remote
                    .notify_pointer_axis_discrete(session, Axis::Vertical, *dy)
                    .await?;
            }
            if *dx != 0 {
                remote
                    .notify_pointer_axis_discrete(session, Axis::Horizontal, *dx)
                    .await?;
            }
        }
        InputEvent::Key { code, pressed } => {
            let state = if *pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            remote
                .notify_keyboard_keycode(session, i32::from(*code), state)
                .await?;
        }
    }
    Ok(())
}

fn pipewire_thread(
    fd: OwnedFd,
    targets: Vec<StreamTarget>,
    tx: tokio::sync::mpsc::Sender<RawFrame>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None).context("pipewire main loop")?;
    let context = pw::context::Context::new(&mainloop).context("pipewire context")?;
    let core = context
        .connect_fd(fd, None)
        .context("pipewire connect_fd")?;

    let mut holders: Vec<StreamHolder> = Vec::new();
    for target in targets {
        let stream = pw::stream::Stream::new(
            &core,
            "omarchy-connect",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .context("pipewire stream")?;
        let data = StreamData {
            display_id: target.display_id,
            format: spa::param::video::VideoInfoRaw::default(),
            tx: tx.clone(),
            announced: false,
        };
        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, _, old, new| {
                tracing::debug!("pipewire stream {old:?} -> {new:?}");
            })
            .param_changed(|_, user, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
                    return;
                };
                if media_type != spa::param::format::MediaType::Video
                    || media_subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if user.format.parse(param).is_err() {
                    return;
                }
                let size = user.format.size();
                tracing::info!(
                    display = user.display_id,
                    format = ?user.format.format(),
                    width = size.width,
                    height = size.height,
                    "pipewire format"
                );
            })
            .process(|stream, user| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(frame) = frame_from_buffer(user.display_id, user.format, datas) else {
                    if !user.announced {
                        user.announced = true;
                        tracing::warn!(
                            display = user.display_id,
                            "portal buffer was not a mapped raw frame (dmabuf?). The stream asked for BGRx, RGBx, NV12, or I420"
                        );
                    }
                    return;
                };
                let _ = user.tx.try_send(frame);
            })
            .register()
            .context("pipewire listener")?;

        let pod = format_pod()?;
        let mut params = [spa::pod::Pod::from_bytes(&pod).context("format pod")?];
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(target.node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .context("connect pipewire stream")?;
        holders.push(StreamHolder {
            _stream: stream,
            _listener: listener,
            _pod: pod,
        });
    }

    while !stop.load(Ordering::Relaxed) {
        if mainloop
            .loop_()
            .iterate(std::time::Duration::from_millis(20))
            < 0
        {
            break;
        }
    }
    drop(holders);
    Ok(())
}

struct StreamData {
    display_id: u32,
    format: spa::param::video::VideoInfoRaw,
    tx: tokio::sync::mpsc::Sender<RawFrame>,
    announced: bool,
}

struct StreamHolder {
    _stream: pw::stream::Stream,
    _listener: pw::stream::StreamListener<StreamData>,
    _pod: Vec<u8>,
}

fn format_pod() -> anyhow::Result<Vec<u8>> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
            spa::param::video::VideoFormat::BGR,
            spa::param::video::VideoFormat::RGB,
            spa::param::video::VideoFormat::NV12,
            spa::param::video::VideoFormat::I420,
            spa::param::video::VideoFormat::YUY2,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            (spa::utils::Rectangle {
                width: 1920,
                height: 1080
            }),
            (spa::utils::Rectangle {
                width: 1,
                height: 1
            }),
            (spa::utils::Rectangle {
                width: 8192,
                height: 8192
            })
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            (spa::utils::Fraction { num: 60, denom: 1 }),
            (spa::utils::Fraction { num: 0, denom: 1 }),
            (spa::utils::Fraction { num: 240, denom: 1 })
        ),
    );
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|err| anyhow::anyhow!("serialize pipewire format: {err:?}"))?;
    Ok(cursor.into_inner())
}

fn frame_from_buffer(
    display_id: u32,
    format: spa::param::video::VideoInfoRaw,
    datas: &mut [spa::buffer::Data],
) -> Option<RawFrame> {
    let size = format.size();
    let width = yuv::even(size.width);
    let height = yuv::even(size.height);
    if width < 2 || height < 2 || datas.is_empty() {
        return None;
    }
    let video_format = format.format();
    let i420 = if video_format == spa::param::video::VideoFormat::I420 && datas.len() >= 3 {
        planar_i420(width, height, datas)?
    } else if video_format == spa::param::video::VideoFormat::NV12 && datas.len() >= 2 {
        let y = owned_plane(&mut datas[0], width as usize)?;
        let uv = owned_plane(&mut datas[1], width as usize)?;
        let mut packed = Vec::with_capacity(y.bytes.len() + uv.bytes.len());
        packed.extend_from_slice(&y.bytes);
        packed.extend_from_slice(&uv.bytes);
        yuv::nv12_to_i420(&packed, y.stride, uv.stride, width, height)?
    } else {
        let plane = owned_plane(&mut datas[0], default_stride(video_format, width))?;
        convert_packed(video_format, &plane.bytes, plane.stride, width, height)?
    };
    Some(RawFrame {
        display_id,
        width,
        height,
        i420,
    })
}

fn planar_i420(width: u32, height: u32, datas: &mut [spa::buffer::Data]) -> Option<Vec<u8>> {
    let y = owned_plane(&mut datas[0], width as usize)?;
    let u = owned_plane(&mut datas[1], width as usize / 2)?;
    let v = owned_plane(&mut datas[2], width as usize / 2)?;
    let mut out = Vec::with_capacity(yuv::i420_size(width, height));
    copy_plane(
        &mut out,
        &y.bytes,
        y.stride,
        width as usize,
        height as usize,
    );
    copy_plane(
        &mut out,
        &u.bytes,
        u.stride,
        width as usize / 2,
        height as usize / 2,
    );
    copy_plane(
        &mut out,
        &v.bytes,
        v.stride,
        width as usize / 2,
        height as usize / 2,
    );
    if out.len() == yuv::i420_size(width, height) {
        Some(out)
    } else {
        None
    }
}

struct OwnedPlane {
    bytes: Vec<u8>,
    stride: usize,
}

fn owned_plane(data: &mut spa::buffer::Data, fallback_stride: usize) -> Option<OwnedPlane> {
    let stride = positive_stride(data.chunk().stride(), fallback_stride);
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let raw = data.data()?;
    let bytes = if size > 0 && offset.saturating_add(size) <= raw.len() {
        raw[offset..offset + size].to_vec()
    } else {
        raw.to_vec()
    };
    if bytes.is_empty() {
        None
    } else {
        Some(OwnedPlane { bytes, stride })
    }
}

fn copy_plane(out: &mut Vec<u8>, src: &[u8], stride: usize, width: usize, height: usize) {
    for row in 0..height {
        let start = row * stride;
        let end = start + width;
        if end <= src.len() {
            out.extend_from_slice(&src[start..end]);
        }
    }
}

fn positive_stride(stride: i32, fallback: usize) -> usize {
    if stride > 0 {
        stride as usize
    } else {
        fallback
    }
}

fn default_stride(format: spa::param::video::VideoFormat, width: u32) -> usize {
    let w = width as usize;
    if format == spa::param::video::VideoFormat::RGB
        || format == spa::param::video::VideoFormat::BGR
    {
        w * 3
    } else if format == spa::param::video::VideoFormat::YUY2 {
        w * 2
    } else if format == spa::param::video::VideoFormat::I420
        || format == spa::param::video::VideoFormat::NV12
    {
        w
    } else {
        w * 4
    }
}

fn convert_packed(
    format: spa::param::video::VideoFormat,
    bytes: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    use spa::param::video::VideoFormat as F;
    if format == F::BGRx {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Bgrx)
    } else if format == F::RGBx {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Rgbx)
    } else if format == F::BGRA {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Bgra)
    } else if format == F::RGBA {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Rgba)
    } else if format == F::BGR {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Bgr)
    } else if format == F::RGB {
        yuv::packed_to_i420(bytes, stride, width, height, PixelOrder::Rgb)
    } else if format == F::YUY2 {
        yuv::yuy2_to_i420(bytes, stride, width, height)
    } else if format == F::I420 {
        yuv::copy_i420(bytes, width, height)
    } else if format == F::NV12 {
        yuv::nv12_to_i420(bytes, stride, stride, width, height)
    } else {
        None
    }
}
