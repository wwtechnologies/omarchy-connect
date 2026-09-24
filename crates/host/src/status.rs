//! `omarchy-connect status`: daemon state merged with settings, for people
//! and for the bar widget (`--json`).

use std::path::Path;

use serde::Serialize;

use crate::settings::Settings;
use crate::state::{unix_now, LiveState, Status};

#[derive(Debug, Serialize)]
pub struct Report {
    pub running: bool,
    /// `stopped`, `listening`, `sharing`, or `connected`.
    pub status: String,
    pub port: Option<u16>,
    pub addresses: Vec<Address>,
    pub peer: Option<String>,
    pub client: Option<String>,
    pub since: Option<u64>,
    pub unattended: bool,
    pub pin_set: bool,
    pub locked_secs: u64,
    pub last_error: Option<String>,
    pub input_ready: bool,
    pub share_saved: bool,
    pub settings_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Address {
    pub interface: String,
    pub ip: String,
}

pub fn report(settings_path: &Path, state_path: &Path, token_path: Option<&Path>) -> Report {
    let live = read_live(state_path).filter(|live| pid_alive(live.pid));
    let (settings, settings_error) = match Settings::load(settings_path) {
        Ok(settings) => (settings, None),
        Err(err) => (Settings::default(), Some(format!("{err:#}"))),
    };
    let now = unix_now();
    let status = match live.as_ref().map(|live| live.status) {
        None => "stopped",
        Some(Status::Listening) => "listening",
        Some(Status::Sharing) => "sharing",
        Some(Status::Connected) => "connected",
    };
    Report {
        running: live.is_some(),
        status: status.into(),
        port: live.as_ref().map(|live| live.port),
        addresses: lan_addresses(),
        peer: live.as_ref().and_then(|live| live.peer.clone()),
        client: live.as_ref().and_then(|live| live.client.clone()),
        since: live.as_ref().and_then(|live| live.since),
        unattended: settings.unattended,
        pin_set: settings.pin.as_deref().is_some_and(|pin| !pin.is_empty()),
        locked_secs: live
            .as_ref()
            .and_then(|live| live.locked_until)
            .map(|until| until.saturating_sub(now))
            .unwrap_or(0),
        last_error: live.as_ref().and_then(|live| live.last_error.clone()),
        input_ready: live.as_ref().is_some_and(|live| live.input_ready),
        share_saved: token_path.is_some_and(Path::exists),
        settings_error,
    }
}

pub fn read_live(state_path: &Path) -> Option<LiveState> {
    let text = std::fs::read(state_path).ok()?;
    serde_json::from_slice(&text).ok()
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(unix))]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0
}

/// IPv4 addresses a LAN client can type in. Container and VM bridges are
/// left out; Tailscale and other VPNs stay.
pub fn lan_addresses() -> Vec<Address> {
    const SKIP: [&str; 5] = ["docker", "br-", "veth", "virbr", "podman"];
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    interfaces
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter(|iface| !SKIP.iter().any(|prefix| iface.name.starts_with(prefix)))
        .filter_map(|iface| match iface.ip() {
            std::net::IpAddr::V4(ip) => Some(Address {
                interface: iface.name,
                ip: ip.to_string(),
            }),
            std::net::IpAddr::V6(_) => None,
        })
        .collect()
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let port = self.port.unwrap_or(crate::DEFAULT_PORT);
        match self.status.as_str() {
            "stopped" => writeln!(f, "Omarchy Connect is not running")?,
            "listening" => writeln!(f, "Omarchy Connect is waiting for a connection")?,
            "sharing" => writeln!(
                f,
                "{} is connecting; waiting for the share picker",
                self.peer.as_deref().unwrap_or("A client")
            )?,
            _ => writeln!(
                f,
                "Connected: {} ({})",
                self.peer.as_deref().unwrap_or("?"),
                self.client.as_deref().unwrap_or("client")
            )?,
        }
        for address in &self.addresses {
            writeln!(f, "  address {}:{port} ({})", address.ip, address.interface)?;
        }
        let access = match (self.unattended, self.pin_set) {
            (true, true) => "on",
            (true, false) => "on, but no PIN is set",
            (false, _) => "off",
        };
        writeln!(f, "Unattended access: {access}")?;
        if self.locked_secs > 0 {
            writeln!(f, "Locked for {} s after wrong PINs", self.locked_secs)?;
        }
        if self.running && !self.input_ready {
            writeln!(f, "Input: /dev/uinput is not writable; remote keyboard and mouse are off")?;
        }
        if let Some(err) = &self.last_error {
            writeln!(f, "Last session: {err}")?;
        }
        if let Some(err) = &self.settings_error {
            writeln!(f, "Settings: {err}")?;
        }
        Ok(())
    }
}
