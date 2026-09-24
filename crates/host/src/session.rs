use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use omarchy_protocol::{
    read_message, write_message, DisplayInfo, FileReceiver, FileSender, FileTransferError,
    InputEvent, Message, ProtocolError, VERSION,
};
use tokio::io::{split, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::capture::{self, RawFrame, SyntheticDesktop};
use crate::encode::Encoder;
use crate::input::{Cursor, Injector};
use crate::tls::{self, Identity};

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub demo: bool,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub download_dir: PathBuf,
    pub offer_file: Option<PathBuf>,
    pub pin_file: Option<PathBuf>,
    pub input: InputMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Auto,
    Portal,
    Uinput,
    None,
}

impl InputMode {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "portal" => Ok(Self::Portal),
            "uinput" => Ok(Self::Uinput),
            "none" => Ok(Self::None),
            other => anyhow::bail!("unknown input mode {other} (auto, portal, uinput, none)"),
        }
    }
}

pub struct HostReady {
    pub addr: SocketAddr,
    pub pin: String,
}

pub async fn run_host(
    config: HostConfig,
    ready: Option<oneshot::Sender<HostReady>>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    if config.fps == 0 {
        anyhow::bail!("fps must be at least 1");
    }
    std::fs::create_dir_all(&config.download_dir)
        .with_context(|| format!("create {}", config.download_dir.display()))?;
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    let addr = listener.local_addr().context("local address")?;
    let identity = tls::generate_identity()?;
    if let Some(path) = &config.pin_file {
        std::fs::write(path, format!("{}\n", identity.pin_hex))
            .with_context(|| format!("write pin file {}", path.display()))?;
    }
    println!("omarchy-connect host");
    println!("listen {addr}");
    println!("pin {}", identity.pin_hex);
    if config.demo {
        println!("capture demo (synthetic displays)");
    } else {
        println!("capture xdg-desktop-portal / pipewire");
    }
    if let Some(tx) = ready {
        let _ = tx.send(HostReady {
            addr,
            pin: identity.pin_hex.clone(),
        });
    }
    let acceptor = TlsAcceptor::from(identity.config.clone());
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            incoming = listener.accept() => {
                let (tcp, peer) = incoming.context("accept")?;
                tracing::info!(%peer, "client connected");
                tcp.set_nodelay(true).ok();
                if let Err(err) = handle_client(tcp, &acceptor, &identity, &config, &cancel).await {
                    tracing::warn!(error = %err, "session ended");
                }
            }
        }
    }
    Ok(())
}

async fn handle_client(
    tcp: TcpStream,
    acceptor: &TlsAcceptor,
    identity: &Identity,
    config: &HostConfig,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let _ = identity;
    let mut tls = acceptor.accept(tcp).await.context("tls handshake")?;
    let hello = read_message(&mut tls).await.context("read hello")?;
    match hello {
        Message::Hello { version, name } if version == VERSION => {
            tracing::info!(%name, "client hello");
        }
        Message::Hello { version, .. } => {
            anyhow::bail!("client protocol version {version}, host is {VERSION}");
        }
        _ => anyhow::bail!("expected hello"),
    }
    write_message(&mut tls, &Message::HelloAck { version: VERSION })
        .await
        .context("write hello ack")?;

    let (reader, mut writer) = split(tls);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Message>(32);
    let (video_tx, mut video_rx) = mpsc::channel::<Message>(2);
    let session_cancel = cancel.child_token();

    let writer_cancel = session_cancel.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = writer_cancel.cancelled() => break,
                msg = ctrl_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if write_message(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
                msg = video_rx.recv() => {
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
    let mut portal_frames: Option<tokio::sync::mpsc::Receiver<RawFrame>> = None;
    // Dropping this sets the PipeWire stop flag. It has to outlive the frame loop.
    #[cfg(target_os = "linux")]
    let mut portal_guard: Option<capture::PortalCapture> = None;

    if config.demo {
        displays = capture::demo_displays();
        injector = build_injector(config.input, true, &displays, None);
    } else {
        #[cfg(target_os = "linux")]
        {
            let mut portal = tokio::select! {
                _ = session_cancel.cancelled() => return Ok(()),
                opened = capture::open_portal() => opened.context(
                    "xdg-desktop-portal capture failed. On Omarchy this needs a Hyprland session and xdg-desktop-portal-hyprland. Use --demo without a portal."
                )?,
            };
            displays = portal.displays.clone();
            portal_frames = Some(portal.take_frames());
            let portal_input =
                if config.input == InputMode::Portal || config.input == InputMode::Auto {
                    portal.take_input()
                } else {
                    None
                };
            injector = build_injector(config.input, false, &displays, portal_input);
            portal_guard = Some(portal);
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("portal capture requires linux. Use --demo.");
        }
    }

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
        portal_frames,
    )
    .await;
    #[cfg(target_os = "linux")]
    drop(portal_guard);
    result
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
    mut portal_frames: Option<tokio::sync::mpsc::Receiver<RawFrame>>,
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
                        send_frame(&mut encoders, &video_tx, &frame, pts, config.fps, config.bitrate_kbps).await?;
                    }
                    pts = pts.wrapping_add(1000 / u64::from(config.fps.max(1)));
                }
            }
            frame = recv_portal(&mut portal_frames) => {
                let Some(frame) = frame else { break };
                if let Some(display) = displays.iter_mut().find(|d| d.id == frame.display_id) {
                    if display.width != frame.width || display.height != frame.height {
                        display.width = frame.width;
                        display.height = frame.height;
                        encoders.remove(&frame.display_id);
                        let _ = ctrl_tx.send(Message::Displays { displays: displays.clone() }).await;
                    }
                }
                send_frame(&mut encoders, &video_tx, &frame, pts, config.fps, config.bitrate_kbps).await?;
                pts = pts.wrapping_add(1);
            }
        }
    }

    session_cancel.cancel();
    reader_task.abort();
    writer_task.abort();
    Ok(())
}

async fn recv_portal(
    frames: &mut Option<tokio::sync::mpsc::Receiver<RawFrame>>,
) -> Option<RawFrame> {
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
    bitrate_kbps: u32,
) -> anyhow::Result<()> {
    if !encoders.contains_key(&frame.display_id) {
        encoders.insert(
            frame.display_id,
            Encoder::open(frame.width, frame.height, fps, bitrate_kbps)?,
        );
    }
    let encoder = encoders.get_mut(&frame.display_id).expect("encoder");
    let encoded = encoder.encode(&frame.i420, pts)?;
    for packet in encoded {
        let msg = Message::Video {
            display_id: frame.display_id,
            pts_ms: u32::try_from(pts).unwrap_or(u32::MAX),
            keyframe: packet.keyframe,
            data: packet.data,
        };
        if video_tx.try_send(msg).is_err() {
            // The writer is behind. Drop the frame so latency stays bounded.
            break;
        }
    }
    Ok(())
}

fn build_injector(
    mode: InputMode,
    demo: bool,
    displays: &[DisplayInfo],
    #[cfg(target_os = "linux")] portal: Option<crate::capture::PortalInput>,
    #[cfg(not(target_os = "linux"))] portal: Option<()>,
) -> Injector {
    match mode {
        InputMode::None => Injector::None,
        InputMode::Portal if demo => {
            tracing::info!("demo mode draws the pointer into the test pattern");
            Injector::None
        }
        InputMode::Auto if demo => Injector::None,
        InputMode::Portal | InputMode::Auto => {
            #[cfg(target_os = "linux")]
            {
                if let Some(portal) = portal {
                    return Injector::Portal(portal);
                }
            }
            let _ = portal;
            open_uinput_or_none(displays)
        }
        InputMode::Uinput => open_uinput_or_none(displays),
    }
}

fn open_uinput_or_none(displays: &[DisplayInfo]) -> Injector {
    #[cfg(target_os = "linux")]
    {
        match crate::input::open_uinput(displays) {
            Ok(device) => return Injector::Uinput(device),
            Err(err) => tracing::warn!(error = %err, "uinput unavailable"),
        }
    }
    let _ = displays;
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
