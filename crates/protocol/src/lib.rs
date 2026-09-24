//! Session protocol for Omarchy Connect.
//!
//! Host and client exchange [`Message`] values on one reliable, ordered byte
//! stream. The applications run that stream inside TLS, and [`auth`] binds a
//! short host PIN to that TLS session. There are no accounts and no relay.
//!
//! The byte layout is documented on [`message`]. [`read_message`] and
//! [`write_message`] add the length prefix. [`file`] is the reliable
//! single-file side channel that shares the same stream as video and input.

pub mod auth;
mod file;
pub mod keys;
mod message;

pub use file::{FileReceiver, FileSender, FileTransferError, FILE_CHUNK_SIZE, FILE_WINDOW_BYTES};
pub use message::{
    read_message, write_message, DisplayInfo, InputEvent, Message, ProtocolError, MAX_FRAME_BYTES,
    VERSION,
};
