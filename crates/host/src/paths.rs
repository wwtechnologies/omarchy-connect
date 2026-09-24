//! XDG locations for the installed host.
//!
//! - `$XDG_CONFIG_HOME/omarchy-connect/settings.json`: unattended PIN and switch.
//! - `$XDG_STATE_HOME/omarchy-connect/`: portal restore token.
//! - `$XDG_RUNTIME_DIR/omarchy-connect/state.json`: live status for the bar.

use std::path::PathBuf;

const APP: &str = "omarchy-connect";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home().map(|home| home.join(fallback)),
    }
}

pub fn config_dir() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config").map(|dir| dir.join(APP))
}

pub fn state_dir() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state").map(|dir| dir.join(APP))
}

pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(APP))
        .or_else(|| state_dir().map(|dir| dir.join("run")))
}

pub fn settings_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("settings.json"))
}

pub fn state_file() -> Option<PathBuf> {
    runtime_dir().map(|dir| dir.join("state.json"))
}
