use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, SyncSender};

use anyhow::Context;
use omarchy_protocol::{
    read_message, write_message, DisplayInfo, FileReceiver, FileSender, FileTransferError,
    InputEvent, Message, ProtocolError, VERSION,
};
use rustls::pki_types::ServerName;
use tokio::io::{split, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::decode::DecoderBank;
use crate::tls;

pub struct SessionConfig {
    pub addr: SocketAddr,
    pub pin: [u8; 32],
    pub download_dir: PathBuf,
    pub send_file: Option<PathBuf>,
    pub stop_after_frames: Option<u32>,
}

pub struct SessionReport {
    pub displays: Vec<DisplayInfo>,
    pub frames_decoded: u32,
    pub displays_with_motion: Vec<u32>,
    pub sent_ok: bool,
    pub received_path: Option<PathBuf>,
}

impl std::fmt::Display for SessionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} displays, {} frames, motion {:?}, sent {}, received {}",
            self.displays.len(),
            self.frames_decoded,
            self.displays_with_motion,
            self.sent_ok,
            self.received_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none".into())
        )
    }
}

pub struct VideoFrame {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub enum UiEvent {
    Status(String),
    Displays(Vec<DisplayInfo>),
    File {
        incoming: bool,
        name: String,
        transferred: u64,
        total: u64,
        done: bool,
    },
    Closed(String),
}

pub enum ClientCommand {
    Input(InputEvent),
    SendFile(PathBuf),
    Disconnect,
}

#[derive(Clone)]
pub struct UiSink {
    pub frames: SyncSender<VideoFrame>,
    pub events: Sender<UiEvent>,
}

pub const DEFAULT_PORT: u16 = 47921;

/// `192.168.1.10` uses port 47921. `192.168.1.10:47921` keeps the given port.
pub fn parse_host(text: &str) -> anyhow::Result<SocketAddr> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("enter the Omarchy IP");
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = text.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    let with_port = if text.contains(':') {
        text.to_string()
    } else {
        format!("{text}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .with_context(|| format!("resolve {text}"))?
        .next()
        .with_context(|| format!("resolve {text}"))
}

pub fn parse_pin(text: &str) -> anyhow::Result<[u8; 32]> {
    let text = text.trim();
    let text = text.strip_prefix("pin ").unwrap_or(text).trim();
    let bytes = hex::decode(text).context("pin is not hex")?;
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_: Vec<u8>| anyhow::anyhow!("pin is {len} bytes, expected 32"))
}

pub async fn run_session(
    config: SessionConfig,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    ui: Option<UiSink>,
) -> anyhow::Result<SessionReport> {
    let outcome = session(&config, &mut commands, ui.as_ref()).await;
    if let Err(err) = &outcome {
        emit(ui.as_ref(), UiEvent::Closed(err.to_string()));
    }
    outcome
}

async fn session(
    config: &SessionConfig,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    ui: Option<&UiSink>,
) -> anyhow::Result<SessionReport> {
    std::fs::create_dir_all(&config.download_dir)
        .with_context(|| format!("create {}", config.download_dir.display()))?;
    emit(
        ui,
        UiEvent::Status(format!("Connecting to {}", config.addr)),
    );
    let tcp = TcpStream::connect(config.addr)
        .await
        .with_context(|| format!("connect {}", config.addr))?;
    tcp.set_nodelay(true).ok();
    let connector = TlsConnector::from(tls::client_config(config.pin));
    let name = ServerName::try_from("omarchy-connect").context("server name")?;
    let mut tls = connector
        .connect(name, tcp)
        .await
        .context("tls handshake (pin mismatch or the host closed)")?;
    write_message(
        &mut tls,
        &Message::Hello {
            version: VERSION,
            name: "omarchy-client".into(),
        },
    )
    .await
    .context("write hello")?;
    match read_message(&mut tls).await.context("read hello ack")? {
        Message::HelloAck { version } if version == VERSION => {}
        Message::HelloAck { version } => {
            anyhow::bail!("host protocol version {version}, client is {VERSION}");
        }
        _ => anyhow::bail!("expected hello ack"),
    }

    let (mut reader, mut writer) = split(tls);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Message>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = ctrl_rx.recv().await {
            if write_message(&mut writer, &msg).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    emit(ui, UiEvent::Status(format!("Connected to {}", config.addr)));

    let mut live = Live::default();
    if let Some(path) = &config.send_file {
        queue_send(&mut live, &ctrl_tx, ui, path).await?;
    }

    let mut decoders = DecoderBank::new();
    let mut closed = String::from("connection closed");
    loop {
        tokio::select! {
            biased;
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    closed = "client closed".into();
                    break;
                };
                match cmd {
                    ClientCommand::Disconnect => {
                        closed = "disconnected".into();
                        break;
                    }
                    ClientCommand::Input(event) => {
                        if ctrl_tx.send(Message::Input(event)).await.is_err() {
                            closed = "connection closed".into();
                            break;
                        }
                    }
                    ClientCommand::SendFile(path) => {
                        if let Err(err) = queue_send(&mut live, &ctrl_tx, ui, &path).await {
                            tracing::warn!(error = %err, "send file");
                            emit(ui, UiEvent::Status(format!("send failed: {err}")));
                        }
                    }
                }
            }
            incoming = read_message(&mut reader) => {
                let msg = match incoming {
                    Ok(msg) => msg,
                    Err(ProtocolError::Closed) => break,
                    Err(err) => {
                        closed = err.to_string();
                        break;
                    }
                };
                if handle_message(&mut live, &mut decoders, &ctrl_tx, ui, config, msg).await? {
                    closed = "session complete".into();
                    break;
                }
            }
        }
        if live.finished(config) {
            closed = "session complete".into();
            break;
        }
    }

    drop(ctrl_tx);
    writer_task.abort();
    emit(ui, UiEvent::Closed(closed.clone()));
    let report = live.report();
    if config.stop_after_frames.is_some() && !live.finished(config) {
        anyhow::bail!(
            "session ended before the requested frames and file transfers finished ({report})"
        );
    }
    let _ = closed;
    Ok(report)
}

async fn handle_message(
    live: &mut Live,
    decoders: &mut DecoderBank,
    ctrl_tx: &mpsc::Sender<Message>,
    ui: Option<&UiSink>,
    config: &SessionConfig,
    msg: Message,
) -> anyhow::Result<bool> {
    match msg {
        Message::Ping { nonce } => {
            ctrl_tx.send(Message::Pong { nonce }).await.ok();
            return Ok(false);
        }
        Message::Pong { .. } => return Ok(false),
        Message::Displays { displays } => {
            live.displays = displays.clone();
            emit(ui, UiEvent::Displays(displays));
            return Ok(false);
        }
        Message::Video {
            display_id, data, ..
        } => {
            match decoders.push(display_id, &data) {
                Ok(Some(frame)) => {
                    note_motion(live, display_id, &frame.rgba);
                    live.frames_decoded = live.frames_decoded.saturating_add(1);
                    if let Some(ui) = ui {
                        let _ = ui.frames.try_send(VideoFrame {
                            display_id,
                            width: frame.width,
                            height: frame.height,
                            rgba: frame.rgba,
                        });
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::debug!(error = %err, display_id, "decode"),
            }
            return Ok(live.finished(config));
        }
        other => {
            match take_file(live, ui, &config.download_dir, &other) {
                Ok(replies) => {
                    for reply in replies {
                        if ctrl_tx.send(reply).await.is_err() {
                            return Ok(true);
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "file transfer");
                    emit(ui, UiEvent::Status(format!("file transfer: {err}")));
                }
            }
            Ok(live.finished(config))
        }
    }
}

async fn queue_send(
    live: &mut Live,
    ctrl_tx: &mpsc::Sender<Message>,
    ui: Option<&UiSink>,
    path: &Path,
) -> anyhow::Result<()> {
    if live.outgoing.is_some() {
        anyhow::bail!("a file send is already in progress");
    }
    let mut sender = FileSender::open(next_client_id(), path)
        .with_context(|| format!("open {}", path.display()))?;
    let msgs = sender.poll()?;
    emit(
        ui,
        UiEvent::File {
            incoming: false,
            name: sender.name().to_string(),
            transferred: sender.acked(),
            total: sender.size(),
            done: false,
        },
    );
    live.outgoing = Some(sender);
    for msg in msgs {
        ctrl_tx.send(msg).await.context("send file offer")?;
    }
    Ok(())
}

struct Live {
    displays: Vec<DisplayInfo>,
    frames_decoded: u32,
    motion: HashSet<u32>,
    last_sample: HashMap<u32, u64>,
    outgoing: Option<FileSender>,
    incoming: Option<FileReceiver>,
    sent_ok: bool,
    saw_offer: bool,
    received_path: Option<PathBuf>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            displays: Vec::new(),
            frames_decoded: 0,
            motion: HashSet::new(),
            last_sample: HashMap::new(),
            outgoing: None,
            incoming: None,
            sent_ok: false,
            saw_offer: false,
            received_path: None,
        }
    }
}

impl Live {
    fn finished(&self, config: &SessionConfig) -> bool {
        let Some(limit) = config.stop_after_frames else {
            return false;
        };
        if self.frames_decoded < limit || self.displays.is_empty() {
            return false;
        }
        if config.send_file.is_some() && !self.sent_ok {
            return false;
        }
        if self.outgoing.is_some() || self.incoming.is_some() {
            return false;
        }
        if self.saw_offer && self.received_path.is_none() {
            return false;
        }
        true
    }

    fn report(&self) -> SessionReport {
        let mut displays_with_motion: Vec<u32> = self.motion.iter().copied().collect();
        displays_with_motion.sort_unstable();
        SessionReport {
            displays: self.displays.clone(),
            frames_decoded: self.frames_decoded,
            displays_with_motion,
            sent_ok: self.sent_ok,
            received_path: self.received_path.clone(),
        }
    }
}

fn note_motion(live: &mut Live, display_id: u32, rgba: &[u8]) {
    let sample = sample_rgba(rgba);
    if let Some(prev) = live.last_sample.insert(display_id, sample) {
        if prev != sample {
            live.motion.insert(display_id);
        }
    }
}

fn sample_rgba(rgba: &[u8]) -> u64 {
    if rgba.is_empty() {
        return 0;
    }
    let mut hash = 0xcbf29ce484222325u64;
    let step = (rgba.len() / 48).max(4);
    let mut index = 0;
    while index < rgba.len() {
        hash ^= rgba[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += step;
    }
    hash
}

fn take_file(
    live: &mut Live,
    ui: Option<&UiSink>,
    download_dir: &Path,
    msg: &Message,
) -> Result<Vec<Message>, FileTransferError> {
    let mut replies = Vec::new();
    if let Some(outgoing) = live.outgoing.as_mut() {
        match outgoing.on_peer(msg) {
            Ok(true) => {
                replies.extend(outgoing.poll()?);
                let done = outgoing.is_complete();
                emit(
                    ui,
                    UiEvent::File {
                        incoming: false,
                        name: outgoing.name().to_string(),
                        transferred: outgoing.acked(),
                        total: outgoing.size(),
                        done,
                    },
                );
                if done {
                    tracing::info!(name = outgoing.name(), "file send confirmed");
                    live.sent_ok = true;
                    live.outgoing = None;
                }
                return Ok(replies);
            }
            Ok(false) => {}
            Err(err) => {
                live.outgoing = None;
                return Err(err);
            }
        }
    }
    if let Message::FileOffer { id, name, size } = msg {
        live.saw_offer = true;
        if live.incoming.is_some() {
            replies.push(Message::FileCancel {
                id: *id,
                reason: "a file is already incoming".into(),
            });
            return Ok(replies);
        }
        let (receiver, accept) = FileReceiver::accept(download_dir, *id, name, *size)?;
        emit(
            ui,
            UiEvent::File {
                incoming: true,
                name: receiver.name().to_string(),
                transferred: 0,
                total: receiver.size(),
                done: false,
            },
        );
        live.incoming = Some(receiver);
        replies.push(accept);
        return Ok(replies);
    }
    if let Some(incoming) = live.incoming.as_mut() {
        match incoming.handle(msg) {
            Ok(more) => {
                replies.extend(more);
                let done = incoming.is_complete();
                emit(
                    ui,
                    UiEvent::File {
                        incoming: true,
                        name: incoming.name().to_string(),
                        transferred: incoming.received(),
                        total: incoming.size(),
                        done,
                    },
                );
                if done {
                    live.received_path = incoming.saved_path().map(|p| p.to_path_buf());
                    live.incoming = None;
                }
            }
            Err(err) => {
                live.incoming = None;
                return Err(err);
            }
        }
    }
    Ok(replies)
}

fn emit(ui: Option<&UiSink>, event: UiEvent) {
    if let Some(ui) = ui {
        let _ = ui.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_host, DEFAULT_PORT};

    #[test]
    fn host_without_port_uses_default() {
        let addr = parse_host("127.0.0.1").unwrap();
        assert_eq!(addr.port(), DEFAULT_PORT);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn host_keeps_explicit_port() {
        let addr = parse_host("10.0.0.8:9").unwrap();
        assert_eq!(addr.port(), 9);
    }

    #[test]
    fn empty_host_is_rejected() {
        assert!(parse_host("  ").is_err());
    }
}

fn next_client_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed) & 0x7fff_ffff
}
