//! Unattended access settings, shared by the daemon and the CLI.
//!
//! SPAKE2 needs the PIN itself, so the file holds it verbatim and is written
//! with mode 0600. The daemon rereads it for every connection.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

fn default_fps() -> u32 {
    15
}

fn default_bitrate_kbps() -> u32 {
    4000
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub unattended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// Capture and encode rate. Applied when the next client connects.
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Encoder target in kilobits per second.
    #[serde(default = "default_bitrate_kbps")]
    pub bitrate_kbps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            unattended: false,
            pin: None,
            fps: default_fps(),
            bitrate_kbps: default_bitrate_kbps(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = path.parent().context("settings path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        let text = serde_json::to_string_pretty(self).context("encode settings")?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Frame rate and bitrate for the next connection, clamped to a usable range.
    pub fn video(&self) -> (u32, u32) {
        (self.fps.clamp(1, 60), self.bitrate_kbps.clamp(500, 50_000))
    }

    /// The PIN a client must prove, or `None` when unattended access is off.
    pub fn active_pin(&self) -> Option<&str> {
        if self.unattended {
            self.pin.as_deref().filter(|pin| !pin.is_empty())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_is_private_and_roundtrips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        let settings = Settings {
            unattended: true,
            pin: Some("482913".into()),
            fps: 60,
            bitrate_kbps: 12_000,
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(settings.active_pin(), Some("482913"));
        assert_eq!(settings.video(), (60, 12_000));
        assert_eq!(Settings::default().video(), (15, 4_000));
        let off = Settings {
            unattended: false,
            ..settings
        };
        assert_eq!(off.active_pin(), None);
    }
}
