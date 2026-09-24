//! One reliable file at a time, in each direction, on the session stream.
//!
//! The sender hashes the file while reading it. Chunks are in order and at
//! most [`FILE_CHUNK_SIZE`] bytes. At most [`FILE_WINDOW_BYTES`] are in flight
//! waiting for a cumulative ack. After the receiver has acked every byte, the
//! sender writes [`Message::FileComplete`] with the SHA-256. The receiver
//! checks that hash, renames the temp file into the download directory, and
//! echoes `FileComplete`. A mismatch or a gap produces [`Message::FileCancel`]
//! and deletes the partial file.
//!
//! Names are reduced to a single path segment of ASCII letters, digits, `.`,
//! `-`, and `_`, so a hostile offer cannot escape the download directory.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::message::Message;

pub const FILE_CHUNK_SIZE: usize = 16 * 1024;
pub const FILE_WINDOW_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("file transfer {id} failed: {reason}")]
    Failed { id: u32, reason: String },
    #[error("hash mismatch for transfer {0}")]
    HashMismatch(u32),
}

#[derive(Debug)]
pub struct FileSender {
    id: u32,
    name: String,
    size: u64,
    file: File,
    next_offset: u64,
    acked: u64,
    offered: bool,
    accepted: bool,
    hasher: Sha256,
    hash: Option<[u8; 32]>,
    complete_sent: bool,
    peer_confirmed: bool,
    failed: Option<String>,
}

impl FileSender {
    pub fn open(id: u32, path: &Path) -> Result<Self, FileTransferError> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let name = sanitize_file_name(
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file.bin"),
        );
        Ok(Self {
            id,
            name,
            size,
            file,
            next_offset: 0,
            acked: 0,
            offered: false,
            accepted: false,
            hasher: Sha256::new(),
            hash: None,
            complete_sent: false,
            peer_confirmed: false,
            failed: None,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn acked(&self) -> u64 {
        self.acked
    }

    pub fn is_complete(&self) -> bool {
        self.peer_confirmed
    }

    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Messages that should be written now. Call again after each peer reply.
    pub fn poll(&mut self) -> Result<Vec<Message>, FileTransferError> {
        if self.failed.is_some() || self.complete_sent {
            return Ok(Vec::new());
        }
        if !self.offered {
            self.offered = true;
            return Ok(vec![Message::FileOffer {
                id: self.id,
                name: self.name.clone(),
                size: self.size,
            }]);
        }
        if !self.accepted {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if self.size == 0 {
            self.finish_hash();
            out.push(self.complete_message());
            self.complete_sent = true;
            return Ok(out);
        }
        while self.next_offset < self.size
            && self.next_offset.saturating_sub(self.acked) < FILE_WINDOW_BYTES
        {
            let room = FILE_WINDOW_BYTES - self.next_offset.saturating_sub(self.acked);
            let n = usize::try_from(
                room.min(FILE_CHUNK_SIZE as u64)
                    .min(self.size - self.next_offset),
            )
            .unwrap_or(FILE_CHUNK_SIZE);
            let mut buf = vec![0u8; n];
            self.file.seek(SeekFrom::Start(self.next_offset))?;
            self.file.read_exact(&mut buf)?;
            self.hasher.update(&buf);
            out.push(Message::FileChunk {
                id: self.id,
                offset: self.next_offset,
                data: buf,
            });
            self.next_offset += n as u64;
        }
        if self.acked == self.size && !self.complete_sent {
            self.finish_hash();
            out.push(self.complete_message());
            self.complete_sent = true;
        }
        Ok(out)
    }

    /// Returns true when `msg` belongs to this transfer.
    pub fn on_peer(&mut self, msg: &Message) -> Result<bool, FileTransferError> {
        match msg {
            Message::FileAccept { id } if *id == self.id => {
                self.accepted = true;
                Ok(true)
            }
            Message::FileAck { id, offset } if *id == self.id => {
                if *offset < self.acked {
                    return Ok(true);
                }
                if *offset > self.next_offset || *offset > self.size {
                    self.fail("ack past sent bytes");
                    return Err(FileTransferError::Failed {
                        id: self.id,
                        reason: "ack past sent bytes".into(),
                    });
                }
                self.acked = *offset;
                Ok(true)
            }
            Message::FileComplete { id, sha256 } if *id == self.id => {
                if self.hash.is_some() && self.hash.as_ref() != Some(sha256) {
                    self.fail("peer hash mismatch");
                    return Err(FileTransferError::HashMismatch(self.id));
                }
                self.peer_confirmed = true;
                Ok(true)
            }
            Message::FileCancel { id, reason } if *id == self.id => {
                self.fail(reason);
                Err(FileTransferError::Failed {
                    id: self.id,
                    reason: reason.clone(),
                })
            }
            _ => Ok(false),
        }
    }

    fn finish_hash(&mut self) {
        if self.hash.is_none() {
            self.hash = Some(self.hasher.clone().finalize().into());
        }
    }

    fn complete_message(&self) -> Message {
        Message::FileComplete {
            id: self.id,
            sha256: self.hash.unwrap_or([0; 32]),
        }
    }

    fn fail(&mut self, reason: &str) {
        self.failed = Some(reason.to_string());
    }
}

#[derive(Debug)]
pub struct FileReceiver {
    id: u32,
    name: String,
    size: u64,
    received: u64,
    temp_path: PathBuf,
    final_path: PathBuf,
    file: Option<File>,
    hasher: Sha256,
    done: bool,
    failed: Option<String>,
}

impl FileReceiver {
    pub fn accept(
        download_dir: &Path,
        offer_id: u32,
        name: &str,
        size: u64,
    ) -> Result<(Self, Message), FileTransferError> {
        fs::create_dir_all(download_dir)?;
        let name = sanitize_file_name(name);
        let temp_path = download_dir.join(format!(".{name}.{offer_id}.partial"));
        let final_path = download_dir.join(&name);
        let file = File::create(&temp_path)?;
        let recv = Self {
            id: offer_id,
            name,
            size,
            received: 0,
            temp_path,
            final_path,
            file: Some(file),
            hasher: Sha256::new(),
            done: false,
            failed: None,
        };
        Ok((recv, Message::FileAccept { id: offer_id }))
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn is_complete(&self) -> bool {
        self.done
    }

    pub fn saved_path(&self) -> Option<&Path> {
        self.done.then_some(self.final_path.as_path())
    }

    pub fn handle(&mut self, msg: &Message) -> Result<Vec<Message>, FileTransferError> {
        if self.failed.is_some() || self.done {
            return Ok(Vec::new());
        }
        match msg {
            Message::FileChunk { id, offset, data } if *id == self.id => {
                if *offset != self.received || self.received + data.len() as u64 > self.size {
                    return self.cancel("chunk out of order");
                }
                let file = self
                    .file
                    .as_mut()
                    .ok_or_else(|| FileTransferError::Failed {
                        id: self.id,
                        reason: "receiver file closed".into(),
                    })?;
                file.write_all(data)?;
                self.hasher.update(data);
                self.received += data.len() as u64;
                Ok(vec![Message::FileAck {
                    id: self.id,
                    offset: self.received,
                }])
            }
            Message::FileComplete { id, sha256 } if *id == self.id => {
                if self.received != self.size {
                    return self.cancel("complete before all bytes");
                }
                let ours: [u8; 32] = self.hasher.clone().finalize().into();
                if ours != *sha256 {
                    let _ = self.cancel("hash mismatch");
                    return Err(FileTransferError::HashMismatch(self.id));
                }
                if let Some(mut file) = self.file.take() {
                    file.flush()?;
                    file.sync_all()?;
                }
                fs::rename(&self.temp_path, &self.final_path)?;
                self.done = true;
                Ok(vec![Message::FileComplete {
                    id: self.id,
                    sha256: *sha256,
                }])
            }
            Message::FileCancel { id, reason } if *id == self.id => {
                self.cleanup();
                self.failed = Some(reason.clone());
                Err(FileTransferError::Failed {
                    id: self.id,
                    reason: reason.clone(),
                })
            }
            _ => Ok(Vec::new()),
        }
    }

    fn cancel(&mut self, reason: &str) -> Result<Vec<Message>, FileTransferError> {
        self.cleanup();
        self.failed = Some(reason.to_string());
        Ok(vec![Message::FileCancel {
            id: self.id,
            reason: reason.to_string(),
        }])
    }

    fn cleanup(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.temp_path);
    }
}

pub fn sanitize_file_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file.bin");
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii() && (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .take(120)
        .collect();
    let clean = clean.trim_matches('.').to_string();
    if clean.is_empty() {
        "file.bin".into()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(src: &Path, download: &Path) -> Result<PathBuf, FileTransferError> {
        let mut sender = FileSender::open(4, src)?;
        let mut receiver: Option<FileReceiver> = None;
        for _ in 0..10_000 {
            if sender.is_complete() {
                break;
            }
            let outgoing = sender.poll()?;
            if outgoing.is_empty() && receiver.is_none() {
                break;
            }
            for msg in outgoing {
                if let Message::FileOffer { id, name, size } = &msg {
                    let (recv, accept) = FileReceiver::accept(download, *id, name, *size)?;
                    receiver = Some(recv);
                    sender.on_peer(&accept)?;
                    continue;
                }
                let recv = receiver.as_mut().expect("offer first");
                let replies = recv.handle(&msg)?;
                for reply in replies {
                    sender.on_peer(&reply)?;
                }
            }
        }
        assert!(
            sender.is_complete(),
            "sender did not finish: {:?}",
            sender.failure()
        );
        let recv = receiver.expect("receiver");
        assert!(recv.is_complete());
        Ok(recv.saved_path().unwrap().to_path_buf())
    }

    #[test]
    fn transfers_bytes_larger_than_the_window() {
        let dir = std::env::temp_dir().join(format!("omarchy-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("payload.bin");
        let bytes: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &bytes).unwrap();
        let down = dir.join("in");
        let saved = drive(&src, &down).unwrap();
        assert_eq!(fs::read(saved).unwrap(), bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transfers_empty_file() {
        let dir = std::env::temp_dir().join(format!("omarchy-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("empty.dat");
        fs::write(&src, b"").unwrap();
        let saved = drive(&src, &dir.join("in")).unwrap();
        assert_eq!(fs::read(saved).unwrap(), b"");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_stops_until_ack() {
        let dir = std::env::temp_dir().join(format!("omarchy-window-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("wide.bin");
        fs::write(&src, vec![1u8; 200_000]).unwrap();
        let mut sender = FileSender::open(1, &src).unwrap();
        let offer = sender.poll().unwrap();
        assert!(matches!(offer[0], Message::FileOffer { .. }));
        sender.on_peer(&Message::FileAccept { id: 1 }).unwrap();
        let first = sender.poll().unwrap();
        let sent: u64 = first
            .iter()
            .map(|m| match m {
                Message::FileChunk { data, .. } => data.len() as u64,
                _ => 0,
            })
            .sum();
        assert!(sent > 0 && sent <= FILE_WINDOW_BYTES);
        assert!(sender.poll().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_escape_in_offer_name() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name(".."), "file.bin");
        assert_eq!(sanitize_file_name("notes (final).txt"), "notesfinal.txt");
    }

    #[test]
    fn bad_hash_does_not_keep_the_file() {
        let dir = std::env::temp_dir().join(format!("omarchy-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut recv, _accept) = FileReceiver::accept(&dir, 9, "a.bin", 4).unwrap();
        let replies = recv
            .handle(&Message::FileChunk {
                id: 9,
                offset: 0,
                data: b"abcd".to_vec(),
            })
            .unwrap();
        assert!(matches!(replies[0], Message::FileAck { offset: 4, .. }));
        let err = recv
            .handle(&Message::FileComplete {
                id: 9,
                sha256: [0; 32],
            })
            .unwrap_err();
        assert!(matches!(err, FileTransferError::HashMismatch(9)));
        assert!(!dir.join("a.bin").exists());
        assert!(!dir.join(".a.bin.9.partial").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
