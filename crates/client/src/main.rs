#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use omarchy_client::{parse_host, parse_pin, run_gui, run_session, GuiLaunch, SessionConfig};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "client", about = "Omarchy Connect client")]
struct Args {
    /// Host address. `192.168.1.10` uses port 47921.
    #[arg(long)]
    connect: Option<String>,
    /// Certificate pin printed by the host. Hex SHA-256, optional `pin ` prefix.
    #[arg(long)]
    pin: Option<String>,
    /// File written by the host `--pin-file`.
    #[arg(long)]
    pin_file: Option<PathBuf>,
    #[arg(long, default_value = "downloads")]
    download_dir: PathBuf,
    /// Send this file to the host after connecting.
    #[arg(long)]
    send_file: Option<PathBuf>,
    /// Decode frames and exit. No window.
    #[arg(long)]
    headless: bool,
    /// With `--headless`, exit after this many decoded frames and any in-flight file.
    #[arg(long, default_value_t = 8)]
    frames: u32,
}

fn main() -> anyhow::Result<()> {
    attach_parent_console();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let pin_text = if let Some(text) = args.pin {
        Some(text)
    } else if let Some(path) = args.pin_file {
        Some(
            std::fs::read_to_string(&path)
                .with_context(|| format!("read pin {}", path.display()))?,
        )
    } else {
        None
    };

    if args.headless {
        let connect = args
            .connect
            .as_deref()
            .context("headless mode needs --connect")?;
        let pin_text = pin_text.as_deref().context("headless mode needs --pin or --pin-file")?;
        let config = SessionConfig {
            addr: parse_host(connect)?,
            pin: parse_pin(pin_text)?,
            download_dir: args.download_dir,
            send_file: args.send_file,
            stop_after_frames: Some(args.frames),
        };
        let runtime = tokio::runtime::Runtime::new().context("tokio runtime")?;
        let (_commands, commands) = tokio::sync::mpsc::unbounded_channel();
        let report = runtime.block_on(run_session(config, commands, None))?;
        println!("displays {}", report.displays.len());
        for display in &report.displays {
            println!(
                "display {} {} {}x{} at {},{} scale {}%",
                display.id,
                display.name,
                display.width,
                display.height,
                display.x,
                display.y,
                display.scale_percent
            );
        }
        println!("frames {}", report.frames_decoded);
        let motion = report
            .displays_with_motion
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        println!("motion {motion}");
        if let Some(path) = &report.received_path {
            println!("received {}", path.display());
        }
        println!("sent {}", if report.sent_ok { "ok" } else { "none" });
        return Ok(());
    }

    let preset = match (&args.connect, &pin_text) {
        (Some(connect), Some(pin_text)) => Some(SessionConfig {
            addr: parse_host(connect)?,
            pin: parse_pin(pin_text)?,
            download_dir: args.download_dir.clone(),
            send_file: args.send_file.clone(),
            stop_after_frames: None,
        }),
        _ => None,
    };
    run_gui(GuiLaunch {
        preset,
        host_text: args.connect.unwrap_or_default(),
        pin_text: pin_text.unwrap_or_default(),
        download_dir: args.download_dir,
        send_file: args.send_file,
    })
}

/// A Windows GUI binary has no console. Reattach when it was started from a terminal
/// so `--help` and `--headless` still print.
fn attach_parent_console() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        #[link(name = "msvcrt")]
        extern "C" {
            fn freopen(filename: *const i8, mode: *const i8, file: *mut std::ffi::c_void)
                -> *mut std::ffi::c_void;
            fn __iob_func() -> *mut std::ffi::c_void;
        }
        if AttachConsole(u32::MAX) == 0 {
            return;
        }
        let name = b"CONOUT$\0";
        let mode = b"w\0";
        // msvcrt FILE is 48 bytes on x64; stdout is index 1, stderr is index 2.
        let iob = __iob_func();
        let stdout = iob.add(48);
        let stderr = iob.add(96);
        freopen(name.as_ptr().cast(), mode.as_ptr().cast(), stdout);
        freopen(name.as_ptr().cast(), mode.as_ptr().cast(), stderr);
    }
}
