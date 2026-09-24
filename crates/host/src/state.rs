//! Live daemon status, published as JSON for `omarchy-connect status` and the
//! bar widget. Written atomically on every change and removed on shutdown.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Listening,
    /// A client proved the PIN and the host is waiting on the share picker.
    Sharing,
    Connected,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiveState {
    pub pid: u32,
    pub status: Status,
    pub port: u16,
    pub peer: Option<String>,
    pub client: Option<String>,
    /// Unix seconds when the current session started.
    pub since: Option<u64>,
    /// Unix seconds until which wrong-PIN lockout refuses attempts.
    pub locked_until: Option<u64>,
    pub last_error: Option<String>,
    pub input_ready: bool,
}

#[derive(Clone)]
pub struct Publisher {
    path: Option<PathBuf>,
    state: Arc<Mutex<LiveState>>,
}

impl Publisher {
    pub fn new(path: Option<PathBuf>, initial: LiveState) -> Self {
        let publisher = Self {
            path,
            state: Arc::new(Mutex::new(initial)),
        };
        publisher.update(|_| {});
        publisher
    }

    pub fn update(&self, change: impl FnOnce(&mut LiveState)) {
        let mut state = self.state.lock().expect("live state");
        change(&mut state);
        let Some(path) = &self.path else { return };
        if let Err(err) = write_atomic(path, &state) {
            tracing::warn!(error = %err, path = %path.display(), "publish state");
        }
    }

    pub fn clear(&self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_atomic(path: &PathBuf, state: &LiveState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
