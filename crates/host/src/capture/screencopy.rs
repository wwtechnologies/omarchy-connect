//! wlr-screencopy capture of every monitor.
//!
//! The portal's share picker in xdg-desktop-portal-hyprland lets one monitor
//! through per session. Hyprland also offers wlr-screencopy to clients in the
//! session with no picker, so the host captures every output this way and uses
//! the portal only when the compositor lacks the protocol. The output list is
//! read once per session; a monitor plugged in mid-session appears on the next.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use omarchy_protocol::DisplayInfo;
use tokio::sync::mpsc;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::capture::RawFrame;
use crate::input::Region;
use crate::yuv::{self, PixelOrder};

/// A sleeping (DPMS off) output may never answer a copy request.
const ROUND_TIMEOUT: Duration = Duration::from_millis(500);

pub struct ScreencopyCapture {
    pub displays: Vec<DisplayInfo>,
    pub regions: Vec<Region>,
    frames: Option<mpsc::Receiver<RawFrame>>,
    stop: Arc<AtomicBool>,
}

impl Drop for ScreencopyCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ScreencopyCapture {
    pub fn take_frames(&mut self) -> mpsc::Receiver<RawFrame> {
        self.frames.take().expect("screencopy frames taken once")
    }
}

type Setup = anyhow::Result<(Vec<DisplayInfo>, Vec<Region>, mpsc::Receiver<RawFrame>)>;

pub async fn open_screencopy(fps: u32) -> anyhow::Result<ScreencopyCapture> {
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<Setup>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::Builder::new()
        .name("omarchy-screencopy".into())
        .spawn(move || {
            let mut grabber = match Grabber::connect() {
                Ok(grabber) => grabber,
                Err(err) => {
                    let _ = setup_tx.send(Err(err));
                    return;
                }
            };
            let (displays, regions) = grabber.layout();
            let (tx, rx) = mpsc::channel(2 * displays.len().max(1));
            if setup_tx.send(Ok((displays, regions, rx))).is_err() {
                return;
            }
            if let Err(err) = grabber.run(fps, tx, stop_thread) {
                tracing::error!(error = %err, "screencopy capture ended");
            }
        })
        .context("spawn screencopy thread")?;
    let (displays, regions, frames) = setup_rx.await.context("screencopy thread ended")??;
    tracing::info!(count = displays.len(), "screencopy capture started");
    Ok(ScreencopyCapture {
        displays,
        regions,
        frames: Some(frames),
        stop,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Spec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct ShmBuffer {
    file: File,
    buffer: wl_buffer::WlBuffer,
    spec: Spec,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Idle,
    Waiting,
    Ready,
    Failed,
}

struct Output {
    wl: wl_output::WlOutput,
    name: Option<String>,
    geometry: (i32, i32),
    mode: Option<(i32, i32)>,
    scale: i32,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    spec: Option<Spec>,
    shm: Option<ShmBuffer>,
    y_invert: bool,
    pending: Pending,
}

impl Output {
    fn new(wl: wl_output::WlOutput) -> Self {
        Self {
            wl,
            name: None,
            geometry: (0, 0),
            mode: None,
            scale: 1,
            logical_position: None,
            logical_size: None,
            frame: None,
            spec: None,
            shm: None,
            y_invert: false,
            pending: Pending::Idle,
        }
    }

    fn logical_rect(&self) -> (i32, i32, u32, u32) {
        let (x, y) = self.logical_position.unwrap_or(self.geometry);
        let (w, h) = self.logical_size.unwrap_or_else(|| {
            let (w, h) = self.mode.unwrap_or((1920, 1080));
            let scale = self.scale.max(1);
            (w / scale, h / scale)
        });
        (x, y, w.max(1) as u32, h.max(1) as u32)
    }
}

struct State {
    shm: wl_shm::WlShm,
    outputs: Vec<Output>,
}

struct Grabber {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    state: State,
    scratch: Vec<u8>,
}

impl Grabber {
    fn connect() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connect to the Wayland session")?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).context("read Wayland globals")?;
        let qh = queue.handle();
        let manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .context("compositor has no wlr-screencopy")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("bind wl_shm")?;
        let xdg: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> =
            globals.bind(&qh, 2..=3, ()).ok();

        let mut outputs = Vec::new();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let index = outputs.len();
                let wl = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    global.version.min(4),
                    &qh,
                    index,
                );
                outputs.push(Output::new(wl));
            }
        }
        if outputs.is_empty() {
            anyhow::bail!("the compositor reported no monitors");
        }
        if let Some(xdg) = &xdg {
            for (index, output) in outputs.iter().enumerate() {
                xdg.get_xdg_output(&output.wl, &qh, index);
            }
        }

        let mut grabber = Self {
            queue,
            qh,
            manager,
            state: State { shm, outputs },
            scratch: Vec::new(),
        };
        grabber
            .queue
            .roundtrip(&mut grabber.state)
            .context("read monitor layout")?;
        grabber
            .queue
            .roundtrip(&mut grabber.state)
            .context("read monitor layout")?;
        grabber.round()?;
        if grabber.state.outputs.iter().all(|o| o.spec.is_none()) {
            anyhow::bail!("screencopy offered no shared-memory buffer");
        }
        Ok(grabber)
    }

    /// Pixel layout for the client plus the logical rects the pointer uses.
    fn layout(&self) -> (Vec<DisplayInfo>, Vec<Region>) {
        let mut regions = Vec::new();
        let mut names = Vec::new();
        for (index, output) in self.state.outputs.iter().enumerate() {
            let Some(spec) = output.spec else { continue };
            let width = yuv::even(spec.width);
            let height = yuv::even(spec.height);
            if width < 2 || height < 2 {
                continue;
            }
            let (x, y, lw, lh) = output.logical_rect();
            regions.push(Region {
                id: index as u32,
                x,
                y,
                width: lw,
                height: lh,
                pixel_width: width,
                pixel_height: height,
            });
            names.push(
                output
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("monitor-{index}")),
            );
        }
        let origins = pixel_origins(&regions);
        let displays = regions
            .iter()
            .zip(origins)
            .zip(names)
            .map(|((region, (x, y)), name)| DisplayInfo {
                id: region.id,
                x,
                y,
                width: region.pixel_width,
                height: region.pixel_height,
                scale_percent: ((region.pixel_width as f64 * 100.0 / region.width as f64).round()
                    as u16)
                    .max(1),
                name,
            })
            .collect();
        (displays, regions)
    }

    fn run(
        &mut self,
        fps: u32,
        tx: mpsc::Sender<RawFrame>,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let interval = Duration::from_secs(1) / fps.max(1);
        let ids: Vec<(usize, u32, u32)> = self
            .layout()
            .0
            .iter()
            .map(|d| (d.id as usize, d.width, d.height))
            .collect();
        while !stop.load(Ordering::Relaxed) {
            if tx.is_closed() {
                break;
            }
            let started = Instant::now();
            if tx.capacity() >= ids.len() {
                self.round()?;
                for &(index, width, height) in &ids {
                    if let Some(frame) = self.frame(index, width, height) {
                        let _ = tx.try_send(frame);
                    }
                }
            }
            if let Some(rest) = interval.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        Ok(())
    }

    /// Asks every output for one frame and waits for all of them.
    fn round(&mut self) -> anyhow::Result<()> {
        for (index, output) in self.state.outputs.iter_mut().enumerate() {
            output.pending = Pending::Waiting;
            output.frame = Some(self.manager.capture_output(1, &output.wl, &self.qh, index));
        }
        let deadline = Instant::now() + ROUND_TIMEOUT;
        while self
            .state
            .outputs
            .iter()
            .any(|o| o.pending == Pending::Waiting)
        {
            if !self.dispatch_until(deadline)? {
                break;
            }
        }
        for output in &mut self.state.outputs {
            if output.pending == Pending::Waiting {
                output.pending = Pending::Failed;
            }
            if let Some(frame) = output.frame.take() {
                frame.destroy();
            }
        }
        Ok(())
    }

    /// Dispatches events until something arrives or the deadline passes.
    fn dispatch_until(&mut self, deadline: Instant) -> anyhow::Result<bool> {
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(true);
        }
        self.queue.flush().context("flush Wayland requests")?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(true);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut fd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(true);
            }
            return Err(err).context("poll the Wayland socket");
        }
        if ready == 0 {
            return Ok(false);
        }
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err).context("read Wayland events"),
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(true)
    }

    fn frame(&mut self, index: usize, width: u32, height: u32) -> Option<RawFrame> {
        let output = &self.state.outputs[index];
        if output.pending != Pending::Ready {
            return None;
        }
        let shm = output.shm.as_ref()?;
        let spec = shm.spec;
        if yuv::even(spec.width) != width || yuv::even(spec.height) != height {
            return None;
        }
        let stride = spec.stride as usize;
        let len = stride * spec.height as usize;
        self.scratch.resize(len, 0);
        shm.file.read_exact_at(&mut self.scratch, 0).ok()?;
        if output.y_invert {
            flip_rows(&mut self.scratch, stride, spec.height as usize);
        }
        let order = pixel_order(spec.format)?;
        let i420 = yuv::packed_to_i420(&self.scratch, stride, width, height, order)?;
        Some(RawFrame {
            display_id: index as u32,
            width,
            height,
            i420,
        })
    }
}

impl State {
    fn attach(
        &mut self,
        index: usize,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        qh: &QueueHandle<State>,
    ) {
        let shm = self.shm.clone();
        let output = &mut self.outputs[index];
        let Some(spec) = output.spec else {
            output.pending = Pending::Failed;
            return;
        };
        if output.shm.as_ref().map(|b| b.spec) != Some(spec) {
            if let Some(old) = output.shm.take() {
                old.buffer.destroy();
            }
            match create_buffer(&shm, spec, qh) {
                Ok(buffer) => output.shm = Some(buffer),
                Err(err) => {
                    tracing::warn!(error = %err, "screencopy buffer");
                    output.pending = Pending::Failed;
                    return;
                }
            }
        }
        if let Some(buffer) = &output.shm {
            frame.copy(&buffer.buffer);
        }
    }
}

fn create_buffer(
    shm: &wl_shm::WlShm,
    spec: Spec,
    qh: &QueueHandle<State>,
) -> anyhow::Result<ShmBuffer> {
    let size = spec.stride as u64 * spec.height as u64;
    let fd = unsafe { libc::memfd_create(c"omarchy-screencopy".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size).context("size screencopy buffer")?;
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        spec.width as i32,
        spec.height as i32,
        spec.stride as i32,
        spec.format,
        qh,
        (),
    );
    pool.destroy();
    Ok(ShmBuffer { file, buffer, spec })
}

fn pixel_order(format: wl_shm::Format) -> Option<PixelOrder> {
    match format {
        wl_shm::Format::Xrgb8888 => Some(PixelOrder::Bgrx),
        wl_shm::Format::Argb8888 => Some(PixelOrder::Bgra),
        wl_shm::Format::Xbgr8888 => Some(PixelOrder::Rgbx),
        wl_shm::Format::Abgr8888 => Some(PixelOrder::Rgba),
        _ => None,
    }
}

/// Pixel origins for the client's overview. Monitors with different scales
/// have logical and pixel layouts that disagree, so each monitor is placed
/// flush against the pixel edges of the monitors left of and above it.
fn pixel_origins(regions: &[Region]) -> Vec<(i32, i32)> {
    let axis = |start: fn(&Region) -> i32,
                logical_len: fn(&Region) -> u32,
                pixel_len: fn(&Region) -> u32| {
        let mut order: Vec<usize> = (0..regions.len()).collect();
        order.sort_by_key(|&i| start(&regions[i]));
        let mut placed = vec![0i32; regions.len()];
        for (n, &i) in order.iter().enumerate() {
            let region = &regions[i];
            let before = order[..n]
                .iter()
                .filter(|&&j| start(&regions[j]) + logical_len(&regions[j]) as i32 <= start(region))
                .map(|&j| placed[j] + pixel_len(&regions[j]) as i32)
                .max();
            placed[i] = before.unwrap_or_else(|| {
                let scale = pixel_len(region) as f64 / logical_len(region).max(1) as f64;
                (start(region) as f64 * scale).round() as i32
            });
        }
        placed
    };
    let xs = axis(|r| r.x, |r| r.width, |r| r.pixel_width);
    let ys = axis(|r| r.y, |r| r.height, |r| r.pixel_height);
    xs.into_iter().zip(ys).collect()
}

fn flip_rows(bytes: &mut [u8], stride: usize, height: usize) {
    for row in 0..height / 2 {
        let (top, bottom) = bytes.split_at_mut((height - 1 - row) * stride);
        top[row * stride..row * stride + stride].swap_with_slice(&mut bottom[..stride]);
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let index = *index;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                if pixel_order(format).is_some() {
                    state.outputs[index].spec = Some(Spec {
                        format,
                        width,
                        height,
                        stride,
                    });
                }
                if frame.version() < 3 {
                    state.attach(index, frame, qh);
                }
            }
            Event::BufferDone => state.attach(index, frame, qh),
            Event::Flags { flags } => {
                state.outputs[index].y_invert = matches!(
                    flags,
                    WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
                );
            }
            Event::Ready { .. } => state.outputs[index].pending = Pending::Ready,
            Event::Failed => state.outputs[index].pending = Pending::Failed,
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = &mut state.outputs[*index];
        match event {
            wl_output::Event::Geometry { x, y, .. } => output.geometry = (x, y),
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => output.mode = Some((width, height)),
            wl_output::Event::Scale { factor } => output.scale = factor,
            wl_output::Event::Name { name } => {
                output.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = &mut state.outputs[*index];
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.logical_position = Some((x, y))
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } => output.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(State: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::{flip_rows, pixel_origins};
    use crate::input::Region;

    fn region(id: u32, x: i32, y: i32, width: u32, height: u32, scale: f64) -> Region {
        Region {
            id,
            x,
            y,
            width,
            height,
            pixel_width: (width as f64 * scale) as u32,
            pixel_height: (height as f64 * scale) as u32,
        }
    }

    #[test]
    fn mixed_scales_pack_side_by_side() {
        let regions = [
            region(0, 0, 0, 1280, 800, 1.5),
            region(1, 1280, 0, 960, 540, 2.0),
        ];
        assert_eq!(pixel_origins(&regions), vec![(0, 0), (1920, 0)]);
    }

    #[test]
    fn stacked_monitors_pack_vertically() {
        let regions = [
            region(0, 0, 1080, 1920, 1080, 1.0),
            region(1, 0, 0, 1280, 720, 2.0),
        ];
        assert_eq!(pixel_origins(&regions), vec![(0, 1440), (0, 0)]);
    }

    #[test]
    fn flip_rows_reverses_row_order() {
        let mut bytes = vec![1, 1, 2, 2, 3, 3];
        flip_rows(&mut bytes, 2, 3);
        assert_eq!(bytes, vec![3, 3, 2, 2, 1, 1]);
    }
}
