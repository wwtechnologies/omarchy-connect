use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use omarchy_protocol::auth::{PinExchange, Side, TLS_EXPORTER_LABEL};
use omarchy_protocol::{
    read_message, write_message, DisplayInfo, FileReceiver, FileSender, FileTransferError,
    InputEvent, Message, ProtocolError, VERSION,
};
use tokio::io::{split, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::capture::{self, CaptureMode, RawFrame, SyntheticDesktop};
use crate::encode::Encoder;
use crate::input::{Cursor, Injector, Region};
use crate::lockout::Lockout;
use crate::settings::Settings;
use crate::state::{unix_now, LiveState, Publisher, Status};
use crate::tls;

/// TLS, hello, and PIN exchange together. A client that stalls here would
/// otherwise hold the single session slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const WRONG_PIN_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub demo: bool,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub download_dir: PathBuf,
    pub offer_file: Option<PathBuf>,
    /// Fixed PIN for this run. Overrides `settings_path`.
    pub pin: Option<String>,
    /// Unattended PIN and switch, reread for every connection.
    pub settings_path: Option<PathBuf>,
    /// Live status JSON for the bar.
    pub state_path: Option<PathBuf>,
    /// Portal restore token, for when capture falls back to the share picker.
    pub restore_token: Option<PathBuf>,
    pub input: InputMode,
    pub capture: CaptureMode,
    /// Desktop notification when a session starts.
    pub notify: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Auto,
    Uinput,
    None,
}

impl InputMode {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "uinput" => Ok(Self::Uinput),
            "none" => Ok(Self::None),
            other => anyhow::bail!("unknown input mode {other} (auto, uinput, none)"),
        }
    }
}

pub struct HostReady {
    pub addr: SocketAddr,
}

type CurrentSession = Arc<Mutex<Option<CancellationToken>>>;

pub async fn run_host(
    config: HostConfig,
    ready: Option<oneshot::Sender<HostReady>>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    if config.fps == 0 {
        anyhow::bail!("fps must be at least 1");
    }
    if let Some(pin) = &config.pin {
        omarchy_protocol::auth::validate_pin(pin).map_err(anyhow::Error::msg)?;
    }
    std::fs::create_dir_all(&config.download_dir)
        .with_context(|| format!("create {}", config.download_dir.display()))?;
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    let addr = listener.local_addr().context("local address")?;
    let acceptor = TlsAcceptor::from(tls::generate_config()?);
    let publisher = Publisher::new(
        config.state_path.clone(),
        LiveState {
            pid: std::process::id(),
            port: addr.port(),
            input_ready: !config.demo && uinput_writable(),
            ..LiveState::default()
        },
    );
    println!("omarchy-connect host");
    println!("listen {addr}");
    if config.pin.is_some() {
        println!("auth fixed PIN from --pin");
    } else if let Some(path) = &config.settings_path {
        println!("auth unattended PIN from {}", path.display());
    } else {
        println!("auth none configured; every client will be refused");
    }
    if config.demo {
        println!("capture demo (synthetic displays)");
    } else {
        println!("capture {:?} (every monitor through wlr-screencopy, else the portal picker)", config.capture);
    }
    if let Some(tx) = ready {
        let _ = tx.send(HostReady { addr });
    }

    let current: CurrentSession = Arc::new(Mutex::new(None));
    spawn_disconnect_signal(current.clone());
    let mut lockout = Lockout::default();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            incoming = listener.accept() => {
                let (tcp, peer) = incoming.context("accept")?;
                tcp.set_nodelay(true).ok();
                let session_config = with_saved_video(&config);
                tracing::info!(
                    %peer,
                    fps = session_config.fps,
                    bitrate_kbps = session_config.bitrate_kbps,
                    "client connected"
                );
                let session_cancel = cancel.child_token();
                *current.lock().expect("current session") = Some(session_cancel.clone());
                let result = handle_client(tcp, peer, &acceptor, &session_config, &mut lockout, &publisher, &session_cancel).await;
                *current.lock().expect("current session") = None;
                if let Err(err) = &result {
                    tracing::warn!(error = %err, "session ended");
                }
                publisher.update(|s| {
                    s.status = Status::Listening;
                    s.peer = None;
                    s.client = None;
                    s.since = None;
                    s.last_error = result.err().map(|err| format!("{err:#}"));
                });
            }
        }
    }
    publisher.clear();
    Ok(())
}

/// Frame rate and bitrate from the bar, falling back to the process flags.
fn with_saved_video(config: &HostConfig) -> HostConfig {
    let mut config = config.clone();
    let Some(path) = config.settings_path.clone() else {
        return config;
    };
    if !path.exists() {
        return config;
    }
    match Settings::load(&path) {
        Ok(settings) => {
            let (fps, bitrate) = settings.video();
            config.fps = fps;
            config.bitrate_kbps = bitrate;
        }
        Err(err) => tracing::warn!(error = %err, "video settings"),
    }
    config
}

/// `omarchy-connect disconnect` sends SIGUSR1; it ends the current session
/// and leaves the listener up.
fn spawn_disconnect_signal(current: CurrentSession) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut usr1) = signal(SignalKind::user_defined1()) else {
            return;
        };
        while usr1.recv().await.is_some() {
            if let Some(token) = current.lock().expect("current session").as_ref() {
                tracing::info!("disconnect requested");
                token.cancel();
            }
        }
    });
    #[cfg(not(unix))]
    let _ = current;
}

fn uinput_writable() -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok()
}

async fn handle_client(
    tcp: TcpStream,
    peer: SocketAddr,
    acceptor: &TlsAcceptor,
    config: &HostConfig,
    lockout: &mut Lockout,
    publisher: &Publisher,
    session_cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let handshake = async {
        let mut tls = acceptor.accept(tcp).await.context("tls handshake")?;
        let name = match read_message(&mut tls).await.context("read hello")? {
            Message::Hello { version, name } if version == VERSION => name,
            Message::Hello { version, .. } => {
                let _ = deny(
                    &mut tls,
                    0,
                    &format!("This host speaks protocol {VERSION}. Update the client."),
                )
                .await;
                anyhow::bail!("client protocol version {version}, host is {VERSION}");
            }
            _ => anyhow::bail!("expected hello"),
        };
        write_message(&mut tls, &Message::HelloAck { version: VERSION })
            .await
            .context("write hello ack")?;
        let binding: [u8; 32] = tls
            .get_ref()
            .1
            .export_keying_material([0u8; 32], TLS_EXPORTER_LABEL, None)
            .context("tls exporter")?;
        let pin = config.pin.clone().or_else(|| {
            let path = config.settings_path.as_ref()?;
            match Settings::load(path) {
                Ok(settings) => settings.active_pin().map(str::to_string),
                Err(err) => {
                    tracing::warn!(error = %err, "settings");
                    None
                }
            }
        });
        authenticate(&mut tls, &binding, pin.as_deref(), lockout, publisher).await?;
        anyhow::Ok((tls, name))
    };
    let (tls, name) = tokio::select! {
        _ = session_cancel.cancelled() => return Ok(()),
        outcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake) => {
            outcome.map_err(|_| anyhow::anyhow!("handshake timed out"))??
        }
    };
    tracing::info!(%peer, client = %name, "client authenticated");
    publisher.update(|s| {
        s.status = if config.demo { Status::Connected } else { Status::Sharing };
        s.peer = Some(peer.ip().to_string());
        s.client = Some(name.clone());
        s.since = Some(unix_now());
        s.last_error = None;
    });
    if config.notify {
        notify(&format!("Remote session from {} ({name})", peer.ip()));
    }

    let (reader, mut writer) = split(tls);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Message>(32);
    let (video_tx, mut video_rx) = mpsc::channel::<Message>(1);
    let session_cancel = session_cancel.clone();

    let writer_cancel = session_cancel.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = writer_cancel.cancelled() => break,
                msg = video_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if write_message(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
                msg = ctrl_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if write_message(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown().await;
    });

    let xfer = Arc::new(Mutex::new(Transfers::default()));
    let cursor = capture::cursor_handle();
    let displays;
    let injector;
    let mut live_frames: Option<capture::FrameInbox> = None;
    // Dropping this sets the PipeWire stop flag. It has to outlive the frame loop.
    #[cfg(target_os = "linux")]
    let mut capture_guard: Option<capture::LiveCapture> = None;

    if config.demo {
        displays = capture::demo_displays();
        let regions: Vec<Region> = displays.iter().map(Region::unscaled).collect();
        injector = build_injector(config.input, true, &regions);
    } else {
        #[cfg(target_os = "linux")]
        {
            let mut live = tokio::select! {
                _ = session_cancel.cancelled() => {
                    writer_task.abort();
                    return Ok(());
                }
                opened = capture::LiveCapture::open(
                    config.capture,
                    config.fps,
                    config.restore_token.as_deref(),
                ) => opened.context(
                    "screen capture failed. On Omarchy this needs Hyprland (wlr-screencopy or xdg-desktop-portal-hyprland). Use --demo without a desktop."
                )?,
            };
            displays = live.displays().to_vec();
            live_frames = Some(live.take_frames());
            injector = build_injector(config.input, false, &live.regions());
            capture_guard = Some(live);
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("live capture requires linux. Use --demo.");
        }
    }
    publisher.update(|s| s.status = Status::Connected);

    let result = drive_session(
        config,
        session_cancel,
        ctrl_tx,
        video_tx,
        reader,
        writer_task,
        xfer,
        cursor,
        displays,
        injector,
        live_frames,
    )
    .await;
    #[cfg(target_os = "linux")]
    drop(capture_guard);
    result
}

async fn authenticate<S>(
    tls: &mut S,
    binding: &[u8],
    pin: Option<&str>,
    lockout: &mut Lockout,
    publisher: &Publisher,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Message::Auth { spake2: client_msg } = read_message(tls).await.context("read auth")?
    else {
        anyhow::bail!("expected auth");
    };
    if let Some(left) = lockout.remaining(Instant::now()) {
        let secs = left.as_secs().max(1) as u32;
        let _ = deny(
            tls,
            secs,
            &format!("Too many wrong PINs. Try again in {secs} s."),
        )
        .await;
        anyhow::bail!("locked out for {secs} s");
    }
    let Some(pin) = pin else {
        let _ = deny(
            tls,
            0,
            "Unattended access is off on this host. Turn it on and set a PIN from Omarchy Connect in the top bar.",
        )
        .await;
        anyhow::bail!("refused: no unattended PIN set");
    };
    let (exchange, host_msg) = PinExchange::start(Side::Host, pin);
    write_message(tls, &Message::Auth { spake2: host_msg })
        .await
        .context("write auth")?;
    let keys = exchange.finish(&client_msg, binding);
    let Message::AuthConfirm { mac } = read_message(tls).await.context("read auth confirm")? else {
        anyhow::bail!("expected auth confirm");
    };
    let verified = keys.as_ref().map(|keys| keys.verify_peer(&mac));
    let Ok(Ok(())) = verified else {
        let lock = lockout.fail(Instant::now());
        if let Some(lock) = lock {
            publisher.update(|s| s.locked_until = Some(unix_now() + lock.as_secs()));
        }
        tokio::time::sleep(WRONG_PIN_DELAY).await;
        let retry = lock.map(|lock| lock.as_secs() as u32).unwrap_or(0);
        let _ = deny(tls, retry, "Wrong PIN.").await;
        anyhow::bail!("wrong PIN");
    };
    lockout.succeed();
    publisher.update(|s| s.locked_until = None);
    let keys = keys.expect("verified keys");
    write_message(
        tls,
        &Message::AuthConfirm {
            mac: keys.confirmation(),
        },
    )
    .await
    .context("write auth confirm")?;
    Ok(())
}

async fn deny<S>(tls: &mut S, retry_after_secs: u32, reason: &str) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_message(
        tls,
        &Message::AuthDenied {
            retry_after_secs,
            reason: reason.into(),
        },
    )
    .await?;
    tls.shutdown().await?;
    Ok(())
}

fn notify(body: &str) {
    let spawned = std::process::Command::new("notify-send")
        .args(["--app-name=Omarchy Connect", "Omarchy Connect", body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_session(
    config: &HostConfig,
    session_cancel: CancellationToken,
    ctrl_tx: mpsc::Sender<Message>,
    video_tx: mpsc::Sender<Message>,
    mut reader: tokio::io::ReadHalf<tokio_rustls::server::TlsStream<TcpStream>>,
    writer_task: JoinHandle<()>,
    xfer: Arc<Mutex<Transfers>>,
    cursor: Arc<Mutex<Cursor>>,
    mut displays: Vec<DisplayInfo>,
    injector: Injector,
    mut live_frames: Option<capture::FrameInbox>,
) -> anyhow::Result<()> {
    ctrl_tx
        .send(Message::Displays {
            displays: displays.clone(),
        })
        .await
        .context("send displays")?;

    if let Some(path) = &config.offer_file {
        let id = next_id(true);
        let mut sender =
            FileSender::open(id, path).with_context(|| format!("open {}", path.display()))?;
        let msgs = sender.poll()?;
        xfer.lock().expect("transfers").outgoing = Some(sender);
        for msg in msgs {
            ctrl_tx.send(msg).await.context("send file offer")?;
        }
    }

    let xfer_reader = xfer.clone();
    let cursor_reader = cursor.clone();
    let ctrl_reader = ctrl_tx.clone();
    let download_dir = config.download_dir.clone();
    let reader_cancel = session_cancel.clone();
    let mut injector = injector;
    let reader_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = reader_cancel.cancelled() => break,
                incoming = read_message(&mut reader) => {
                    let msg = match incoming {
                        Ok(msg) => msg,
                        Err(ProtocolError::Closed) => break,
                        Err(err) => {
                            tracing::debug!(error = %err, "reader closed");
                            break;
                        }
                    };
                    if let Message::Ping { nonce } = msg {
                        let _ = ctrl_reader.send(Message::Pong { nonce }).await;
                        continue;
                    }
                    if let Message::Input(event) = &msg {
                        if let InputEvent::MouseMove { display_id, x, y } = event {
                            if let Ok(mut cursor) = cursor_reader.lock() {
                                *cursor = Cursor { display_id: *display_id, x: *x, y: *y, visible: true };
                            }
                        }
                        injector.apply(event).await;
                    }
                    match take_file_replies(&xfer_reader, &download_dir, &msg) {
                        Ok(replies) => {
                            for reply in replies {
                                if ctrl_reader.send(reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(err) => tracing::warn!(error = %err, "file transfer"),
                    }
                }
            }
        }
        reader_cancel.cancel();
    });

    let mut encoders: HashMap<u32, Encoder> = HashMap::new();
    let mut bitrate = Bitrate::new(config.bitrate_kbps);
    let mut pts = 0u64;
    let mut tick =
        tokio::time::interval(Duration::from_millis(1000 / u64::from(config.fps.max(1))));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut synthetic = if config.demo {
        Some(SyntheticDesktop::new(cursor.clone()))
    } else {
        None
    };

    loop {
        tokio::select! {
            _ = session_cancel.cancelled() => break,
            _ = tick.tick(), if synthetic.is_some() => {
                if let Some(desktop) = synthetic.as_mut() {
                    for frame in desktop.render() {
                        send_frame(&mut encoders, &video_tx, &frame, pts, config.fps, &mut bitrate).await?;
                    }
                    pts = pts.wrapping_add(1000 / u64::from(config.fps.max(1)));
                }
            }
            frame = recv_live(&mut live_frames) => {
                let Some(frame) = frame else { break };
                if let Some(display) = displays.iter_mut().find(|d| d.id == frame.display_id) {
                    if display.width != frame.width || display.height != frame.height {
                        display.width = frame.width;
                        display.height = frame.height;
                        encoders.remove(&frame.display_id);
                        let _ = ctrl_tx.send(Message::Displays { displays: displays.clone() }).await;
                    }
                }
                send_frame(&mut encoders, &video_tx, &frame, pts, config.fps, &mut bitrate).await?;
                pts = pts.wrapping_add(1);
            }
        }
    }

    session_cancel.cancel();
    reader_task.abort();
    writer_task.abort();
    Ok(())
}

async fn recv_live(frames: &mut Option<capture::FrameInbox>) -> Option<RawFrame> {
    match frames {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn send_frame(
    encoders: &mut HashMap<u32, Encoder>,
    video_tx: &mpsc::Sender<Message>,
    frame: &RawFrame,
    pts: u64,
    fps: u32,
    bitrate: &mut Bitrate,
) -> anyhow::Result<()> {
    let bitrate_kbps = bitrate.choose(frame.width, frame.height, fps);
    if !encoders.contains_key(&frame.display_id) {
        encoders.insert(
            frame.display_id,
            Encoder::open(frame.width, frame.height, fps, bitrate_kbps)?,
        );
    }
    let encoder = encoders.get_mut(&frame.display_id).expect("encoder");
    let encoded = encoder.encode(&frame.i420, pts)?;
    let mut waited = Duration::ZERO;
    for packet in encoded {
        let msg = Message::Video {
            display_id: frame.display_id,
            pts_ms: u32::try_from(pts).unwrap_or(u32::MAX),
            keyframe: packet.keyframe,
            data: packet.data,
        };
        // Blocking keeps the H.264 sequence intact. The capture side already
        // replaced any older picture, so this wait is one frame, not a backlog.
        let started = Instant::now();
        if video_tx.send(msg).await.is_err() {
            break;
        }
        waited += started.elapsed();
    }
    if bitrate.observe(waited, fps) {
        encoders.clear();
    }
    Ok(())
}

/// Fixed kilobits per second, or automatic. Automatic starts from the monitor
/// size and steps down when the client cannot take frames as fast as they are
/// produced, then back up once there is spare time.
struct Bitrate {
    auto: bool,
    kbps: u32,
    ceiling: u32,
    late: u32,
    spare: u32,
}

impl Bitrate {
    fn new(setting: u32) -> Self {
        if setting == 0 {
            Self {
                auto: true,
                kbps: 0,
                ceiling: 50_000,
                late: 0,
                spare: 0,
            }
        } else {
            let kbps = setting.clamp(500, 50_000);
            Self {
                auto: false,
                kbps,
                ceiling: kbps,
                late: 0,
                spare: 0,
            }
        }
    }

    fn choose(&mut self, width: u32, height: u32, fps: u32) -> u32 {
        if self.kbps == 0 {
            let pixels = u64::from(width) * u64::from(height) * u64::from(fps.max(1));
            // About 0.2 bits per pixel. 1080p60 lands near 25 Mb/s, 1440p60 near 44.
            self.kbps = u32::try_from(pixels * 20 / 100_000)
                .unwrap_or(50_000)
                .clamp(8_000, self.ceiling);
            tracing::info!(kbps = self.kbps, "automatic bitrate");
        }
        self.kbps
    }

    /// Returns true when the encoder should be reopened at the new rate.
    fn observe(&mut self, waited: Duration, fps: u32) -> bool {
        if !self.auto {
            return false;
        }
        let budget = Duration::from_millis(1000 / u64::from(fps.max(1)));
        if waited > budget {
            self.spare = 0;
            self.late = self.late.saturating_add(1);
            if self.late >= 6 && self.kbps > 4_000 {
                self.kbps = (self.kbps * 3 / 4).max(4_000);
                self.late = 0;
                tracing::info!(kbps = self.kbps, "lowering bitrate, the client is behind");
                return true;
            }
        } else {
            self.late = 0;
            self.spare = self.spare.saturating_add(1);
            let settle = fps.max(1).saturating_mul(8);
            if self.spare >= settle && self.kbps < self.ceiling {
                let raised = self.kbps.saturating_mul(5) / 4;
                self.kbps = raised.min(self.ceiling);
                self.spare = 0;
                tracing::info!(kbps = self.kbps, "raising bitrate");
                return true;
            }
        }
        false
    }
}

fn build_injector(mode: InputMode, demo: bool, regions: &[Region]) -> Injector {
    match mode {
        InputMode::None => Injector::None,
        InputMode::Auto if demo => {
            tracing::info!("demo mode draws the pointer into the test pattern");
            Injector::None
        }
        InputMode::Auto | InputMode::Uinput => open_uinput_or_none(regions),
    }
}

fn open_uinput_or_none(regions: &[Region]) -> Injector {
    #[cfg(target_os = "linux")]
    {
        match crate::input::open_uinput(regions) {
            Ok(device) => return Injector::Uinput(device),
            Err(err) => tracing::warn!(error = %err, "uinput unavailable"),
        }
    }
    let _ = regions;
    Injector::None
}

struct Transfers {
    outgoing: Option<FileSender>,
    incoming: Option<FileReceiver>,
}

impl Default for Transfers {
    fn default() -> Self {
        Self {
            outgoing: None,
            incoming: None,
        }
    }
}

fn take_file_replies(
    xfer: &Arc<Mutex<Transfers>>,
    download_dir: &PathBuf,
    msg: &Message,
) -> Result<Vec<Message>, FileTransferError> {
    let mut guard = xfer.lock().expect("transfers");
    let mut replies = Vec::new();
    if let Some(outgoing) = guard.outgoing.as_mut() {
        match outgoing.on_peer(msg) {
            Ok(true) => {
                replies.extend(outgoing.poll()?);
                if outgoing.is_complete() {
                    tracing::info!(name = outgoing.name(), "file send confirmed");
                    guard.outgoing = None;
                }
                return Ok(replies);
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(error = %err, "outgoing file");
                guard.outgoing = None;
                return Err(err);
            }
        }
    }
    if let Message::FileOffer { id, name, size } = msg {
        if guard.incoming.is_some() {
            replies.push(Message::FileCancel {
                id: *id,
                reason: "a file is already incoming".into(),
            });
            return Ok(replies);
        }
        let (receiver, accept) = FileReceiver::accept(download_dir, *id, name, *size)?;
        guard.incoming = Some(receiver);
        replies.push(accept);
        return Ok(replies);
    }
    if let Some(incoming) = guard.incoming.as_mut() {
        match incoming.handle(msg) {
            Ok(more) => {
                replies.extend(more);
                if incoming.is_complete() {
                    if let Some(path) = incoming.saved_path() {
                        tracing::info!(path = %path.display(), "file received");
                    }
                    guard.incoming = None;
                }
            }
            Err(err) => {
                guard.incoming = None;
                return Err(err);
            }
        }
    }
    Ok(replies)
}

fn next_id(host: bool) -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed) & 0x7fff_ffff;
    if host {
        id | 0x8000_0000
    } else {
        id
    }
}
