//! Byte layout of an Omarchy Connect session.
//!
//! Multi-byte integers are little-endian. Strings are UTF-8 with a `u16` byte
//! length. The length counts bytes, not scalars, and must be at most 4096.
//!
//! # Framing
//!
//! Each message on the stream is:
//!
//! ```text
//! u32 payload_len   // bytes that follow, 1..=MAX_FRAME_BYTES
//! u8  type
//! [payload_len - 1 bytes of body]
//! ```
//!
//! # Types
//!
//! | type | name          | body |
//! |------|---------------|------|
//! | 1    | Hello         | `u16 version`, `string name` |
//! | 2    | HelloAck      | `u16 version` |
//! | 3    | Displays      | `u16 count`, then `count` displays (max 16) |
//! | 4    | Video         | `u32 display_id`, `u32 pts_ms`, `u8 flags`, `bytes nal` |
//! | 5    | Input         | `u8 kind`, then a kind-specific body |
//! | 6    | FileOffer     | `u32 id`, `u64 size`, `string name` |
//! | 7    | FileAccept    | `u32 id` |
//! | 8    | FileChunk     | `u32 id`, `u64 offset`, `bytes data` |
//! | 9    | FileAck       | `u32 id`, `u64 offset` (cumulative end offset) |
//! | 10   | FileComplete  | `u32 id`, 32-byte SHA-256 |
//! | 11   | FileCancel    | `u32 id`, `string reason` |
//! | 12   | Ping          | `u64 nonce` |
//! | 13   | Pong          | `u64 nonce` |
//!
//! A display is `u32 id`, `i32 x`, `i32 y`, `u32 width`, `u32 height`,
//! `u16 scale_percent` (100 = 1.0), `string name`. `x` and `y` are the
//! display origin in desktop pixels. `width` and `height` are the video
//! framebuffer size in pixels. `scale_percent` is the display scale the host
//! reports; the video itself is already in pixels.
//!
//! Video `flags` bit 0 marks a keyframe (IDR, with parameter sets). Other bits
//! are ignored by version 1 decoders. `nal` is one Annex-B access unit.
//!
//! Input kinds:
//!
//! | kind | name        | body |
//! |------|-------------|------|
//! | 1    | MouseMove   | `u32 display_id`, `u32 x`, `u32 y` (pixels in that display) |
//! | 2    | MouseButton | `u32 display_id`, `u8 button` (0 left, 1 right, 2 middle), `u8 pressed` |
//! | 3    | MouseWheel  | `u32 display_id`, `i32 dx`, `i32 dy` (detents, not pixels) |
//! | 4    | Key         | `u16 code` (Linux evdev), `u8 pressed` |
//!
//! `pressed` is 1 or 0. Any other value is rejected.
//!
//! File transfer is reliable on top of this stream: the sender offers one
//! file, waits for accept, then sends in-order chunks. The receiver acks the
//! cumulative byte offset. After the last byte is acked the sender sends
//! `FileComplete` with the SHA-256 of the file bytes. The receiver checks the
//! hash and echoes `FileComplete`, or sends `FileCancel`. One transfer may be
//! in flight in each direction. See [`crate::file`].

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_STRING_BYTES: usize = 4096;
const MAX_DISPLAYS: usize = 16;

const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_DISPLAYS: u8 = 3;
const T_VIDEO: u8 = 4;
const T_INPUT: u8 = 5;
const T_FILE_OFFER: u8 = 6;
const T_FILE_ACCEPT: u8 = 7;
const T_FILE_CHUNK: u8 = 8;
const T_FILE_ACK: u8 = 9;
const T_FILE_COMPLETE: u8 = 10;
const T_FILE_CANCEL: u8 = 11;
const T_PING: u8 = 12;
const T_PONG: u8 = 13;

const I_MOVE: u8 = 1;
const I_BUTTON: u8 = 2;
const I_WHEEL: u8 = 3;
const I_KEY: u8 = 4;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("connection closed")]
    Closed,
    #[error("frame length {0} is outside 1..={MAX_FRAME_BYTES}")]
    BadLength(u32),
    #[error("truncated message")]
    Truncated,
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("invalid message field")]
    Invalid,
    #[error("message text is not utf-8")]
    Utf8,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// 100 means scale 1.0, 150 means 1.5.
    pub scale_percent: u16,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove {
        display_id: u32,
        x: u32,
        y: u32,
    },
    MouseButton {
        display_id: u32,
        button: u8,
        pressed: bool,
    },
    MouseWheel {
        display_id: u32,
        dx: i32,
        dy: i32,
    },
    Key {
        code: u16,
        pressed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello {
        version: u16,
        name: String,
    },
    HelloAck {
        version: u16,
    },
    Displays {
        displays: Vec<DisplayInfo>,
    },
    Video {
        display_id: u32,
        pts_ms: u32,
        keyframe: bool,
        data: Vec<u8>,
    },
    Input(InputEvent),
    FileOffer {
        id: u32,
        name: String,
        size: u64,
    },
    FileAccept {
        id: u32,
    },
    FileChunk {
        id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    FileAck {
        id: u32,
        offset: u64,
    },
    FileComplete {
        id: u32,
        sha256: [u8; 32],
    },
    FileCancel {
        id: u32,
        reason: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

impl Message {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Message::Hello { version, name } => {
                out.push(T_HELLO);
                put_u16(out, *version);
                put_str(out, name);
            }
            Message::HelloAck { version } => {
                out.push(T_HELLO_ACK);
                put_u16(out, *version);
            }
            Message::Displays { displays } => {
                out.push(T_DISPLAYS);
                put_u16(out, displays.len() as u16);
                for display in displays {
                    put_u32(out, display.id);
                    put_i32(out, display.x);
                    put_i32(out, display.y);
                    put_u32(out, display.width);
                    put_u32(out, display.height);
                    put_u16(out, display.scale_percent);
                    put_str(out, &display.name);
                }
            }
            Message::Video {
                display_id,
                pts_ms,
                keyframe,
                data,
            } => {
                out.push(T_VIDEO);
                put_u32(out, *display_id);
                put_u32(out, *pts_ms);
                out.push(u8::from(*keyframe));
                put_bytes(out, data);
            }
            Message::Input(event) => {
                out.push(T_INPUT);
                match event {
                    InputEvent::MouseMove { display_id, x, y } => {
                        out.push(I_MOVE);
                        put_u32(out, *display_id);
                        put_u32(out, *x);
                        put_u32(out, *y);
                    }
                    InputEvent::MouseButton {
                        display_id,
                        button,
                        pressed,
                    } => {
                        out.push(I_BUTTON);
                        put_u32(out, *display_id);
                        out.push(*button);
                        out.push(u8::from(*pressed));
                    }
                    InputEvent::MouseWheel { display_id, dx, dy } => {
                        out.push(I_WHEEL);
                        put_u32(out, *display_id);
                        put_i32(out, *dx);
                        put_i32(out, *dy);
                    }
                    InputEvent::Key { code, pressed } => {
                        out.push(I_KEY);
                        put_u16(out, *code);
                        out.push(u8::from(*pressed));
                    }
                }
            }
            Message::FileOffer { id, name, size } => {
                out.push(T_FILE_OFFER);
                put_u32(out, *id);
                put_u64(out, *size);
                put_str(out, name);
            }
            Message::FileAccept { id } => {
                out.push(T_FILE_ACCEPT);
                put_u32(out, *id);
            }
            Message::FileChunk { id, offset, data } => {
                out.push(T_FILE_CHUNK);
                put_u32(out, *id);
                put_u64(out, *offset);
                put_bytes(out, data);
            }
            Message::FileAck { id, offset } => {
                out.push(T_FILE_ACK);
                put_u32(out, *id);
                put_u64(out, *offset);
            }
            Message::FileComplete { id, sha256 } => {
                out.push(T_FILE_COMPLETE);
                put_u32(out, *id);
                out.extend_from_slice(sha256);
            }
            Message::FileCancel { id, reason } => {
                out.push(T_FILE_CANCEL);
                put_u32(out, *id);
                put_str(out, reason);
            }
            Message::Ping { nonce } => {
                out.push(T_PING);
                put_u64(out, *nonce);
            }
            Message::Pong { nonce } => {
                out.push(T_PONG);
                put_u64(out, *nonce);
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(buf);
        let kind = r.u8()?;
        let msg = match kind {
            T_HELLO => Message::Hello {
                version: r.u16()?,
                name: r.str()?,
            },
            T_HELLO_ACK => Message::HelloAck { version: r.u16()? },
            T_DISPLAYS => {
                let count = r.u16()? as usize;
                if count > MAX_DISPLAYS {
                    return Err(ProtocolError::Invalid);
                }
                let mut displays = Vec::with_capacity(count);
                for _ in 0..count {
                    let display = DisplayInfo {
                        id: r.u32()?,
                        x: r.i32()?,
                        y: r.i32()?,
                        width: r.u32()?,
                        height: r.u32()?,
                        scale_percent: r.u16()?,
                        name: r.str()?,
                    };
                    if display.width == 0 || display.height == 0 || display.scale_percent == 0 {
                        return Err(ProtocolError::Invalid);
                    }
                    displays.push(display);
                }
                Message::Displays { displays }
            }
            T_VIDEO => {
                let display_id = r.u32()?;
                let pts_ms = r.u32()?;
                let flags = r.u8()?;
                let data = r.bytes()?.to_vec();
                Message::Video {
                    display_id,
                    pts_ms,
                    keyframe: flags & 1 == 1,
                    data,
                }
            }
            T_INPUT => {
                let input_kind = r.u8()?;
                let event = match input_kind {
                    I_MOVE => InputEvent::MouseMove {
                        display_id: r.u32()?,
                        x: r.u32()?,
                        y: r.u32()?,
                    },
                    I_BUTTON => {
                        let display_id = r.u32()?;
                        let button = r.u8()?;
                        let pressed = match r.u8()? {
                            0 => false,
                            1 => true,
                            _ => return Err(ProtocolError::Invalid),
                        };
                        InputEvent::MouseButton {
                            display_id,
                            button,
                            pressed,
                        }
                    }
                    I_WHEEL => InputEvent::MouseWheel {
                        display_id: r.u32()?,
                        dx: r.i32()?,
                        dy: r.i32()?,
                    },
                    I_KEY => {
                        let code = r.u16()?;
                        let pressed = match r.u8()? {
                            0 => false,
                            1 => true,
                            _ => return Err(ProtocolError::Invalid),
                        };
                        InputEvent::Key { code, pressed }
                    }
                    _ => return Err(ProtocolError::Invalid),
                };
                Message::Input(event)
            }
            T_FILE_OFFER => Message::FileOffer {
                id: r.u32()?,
                size: r.u64()?,
                name: r.str()?,
            },
            T_FILE_ACCEPT => Message::FileAccept { id: r.u32()? },
            T_FILE_CHUNK => Message::FileChunk {
                id: r.u32()?,
                offset: r.u64()?,
                data: r.bytes()?.to_vec(),
            },
            T_FILE_ACK => Message::FileAck {
                id: r.u32()?,
                offset: r.u64()?,
            },
            T_FILE_COMPLETE => {
                let id = r.u32()?;
                let raw = r.take(32)?;
                let mut sha256 = [0u8; 32];
                sha256.copy_from_slice(raw);
                Message::FileComplete { id, sha256 }
            }
            T_FILE_CANCEL => Message::FileCancel {
                id: r.u32()?,
                reason: r.str()?,
            },
            T_PING => Message::Ping { nonce: r.u64()? },
            T_PONG => Message::Pong { nonce: r.u64()? },
            other => return Err(ProtocolError::UnknownType(other)),
        };
        if !r.is_empty() {
            return Err(ProtocolError::Invalid);
        }
        Ok(msg)
    }
}

pub async fn read_message<R>(reader: &mut R) -> Result<Message, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ProtocolError::Closed);
        }
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_le_bytes(len_buf);
    if len == 0 || len as usize > MAX_FRAME_BYTES {
        return Err(ProtocolError::BadLength(len));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Message::decode(&buf)
}

pub async fn write_message<W>(writer: &mut W, msg: &Message) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    let mut body = Vec::new();
    msg.encode(&mut body);
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::Invalid);
    }
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.pos.checked_add(n).ok_or(ProtocolError::Truncated)?;
        if end > self.buf.len() {
            return Err(ProtocolError::Truncated);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(self.u32()? as i32)
    }

    fn bytes(&mut self) -> Result<&'a [u8], ProtocolError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::Invalid);
        }
        self.take(len)
    }

    fn str(&mut self) -> Result<String, ProtocolError> {
        let len = self.u16()? as usize;
        if len > MAX_STRING_BYTES {
            return Err(ProtocolError::Invalid);
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::Utf8)
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    put_u32(out, v as u32);
}

fn put_bytes(out: &mut Vec<u8>, data: &[u8]) {
    put_u32(out, data.len() as u32);
    out.extend_from_slice(data);
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(MAX_STRING_BYTES);
    put_u16(out, len as u16);
    out.extend_from_slice(&bytes[..len]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        let decoded = Message::decode(&buf).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn every_message_roundtrips() {
        roundtrip(Message::Hello {
            version: VERSION,
            name: "omarchy-client".into(),
        });
        roundtrip(Message::HelloAck { version: VERSION });
        roundtrip(Message::Displays {
            displays: vec![
                DisplayInfo {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: 960,
                    height: 540,
                    scale_percent: 100,
                    name: "eDP-1".into(),
                },
                DisplayInfo {
                    id: 1,
                    x: 960,
                    y: -40,
                    width: 1920,
                    height: 1080,
                    scale_percent: 150,
                    name: "HDMI-A-1".into(),
                },
            ],
        });
        roundtrip(Message::Video {
            display_id: 1,
            pts_ms: 33,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
        });
        roundtrip(Message::Input(InputEvent::MouseMove {
            display_id: 0,
            x: 10,
            y: 20,
        }));
        roundtrip(Message::Input(InputEvent::MouseButton {
            display_id: 0,
            button: 1,
            pressed: true,
        }));
        roundtrip(Message::Input(InputEvent::MouseWheel {
            display_id: 1,
            dx: -1,
            dy: 2,
        }));
        roundtrip(Message::Input(InputEvent::Key {
            code: crate::keys::KEY_A,
            pressed: false,
        }));
        roundtrip(Message::FileOffer {
            id: 7,
            name: "notes.txt".into(),
            size: 99,
        });
        roundtrip(Message::FileAccept { id: 7 });
        roundtrip(Message::FileChunk {
            id: 7,
            offset: 16,
            data: b"hello".to_vec(),
        });
        roundtrip(Message::FileAck { id: 7, offset: 21 });
        roundtrip(Message::FileComplete {
            id: 7,
            sha256: [0xab; 32],
        });
        roundtrip(Message::FileCancel {
            id: 7,
            reason: "busy".into(),
        });
        roundtrip(Message::Ping { nonce: 42 });
        roundtrip(Message::Pong { nonce: 42 });
    }

    #[test]
    fn rejects_trailing_bytes_and_bad_pressed_flag() {
        let mut buf = Vec::new();
        Message::Ping { nonce: 1 }.encode(&mut buf);
        buf.push(0);
        assert!(Message::decode(&buf).is_err());

        let mut bad = Vec::new();
        bad.push(T_INPUT);
        bad.push(I_KEY);
        put_u16(&mut bad, 30);
        bad.push(2);
        assert!(matches!(Message::decode(&bad), Err(ProtocolError::Invalid)));
    }

    #[tokio::test]
    async fn framed_write_and_read() {
        let msg = Message::Hello {
            version: VERSION,
            name: "desk".into(),
        };
        let (mut client, mut server) = tokio::io::duplex(256);
        write_message(&mut client, &msg).await.unwrap();
        let got = read_message(&mut server).await.unwrap();
        assert_eq!(got, msg);
    }
}
