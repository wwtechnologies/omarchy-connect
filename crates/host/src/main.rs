use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use omarchy_host::settings::Settings;
use omarchy_host::{paths, run_host, status, CaptureMode, HostConfig, InputMode};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "omarchy-connect",
    about = "Omarchy Connect host. With no command, runs the host.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the host (the default).
    Serve(ServeArgs),
    /// Show whether the host is running, its addresses, and unattended access.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Set or clear the unattended access PIN.
    Pin {
        #[command(subcommand)]
        action: PinAction,
    },
    /// Turn unattended access on or off. The PIN is kept either way.
    Unattended { state: OnOff },
    /// End the current remote session. The host keeps listening.
    Disconnect,
    /// Forget the saved portal screen share so its picker appears next time.
    ResetShare,
}

#[derive(Subcommand)]
enum PinAction {
    /// Set the PIN and turn unattended access on. Reads stdin when PIN is omitted.
    Set { pin: Option<String> },
    /// Remove the PIN and turn unattended access off.
    Clear,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(clap::Args, Clone)]
struct ServeArgs {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:47921")]
    bind: SocketAddr,
    /// Two synthetic monitors instead of capturing the desktop.
    #[arg(long)]
    demo: bool,
    #[arg(long, default_value_t = 15)]
    fps: u32,
    #[arg(long, default_value_t = 4000)]
    bitrate_kbps: u32,
    /// Where files sent by the client land. Defaults to ~/Downloads/Omarchy Connect.
    #[arg(long)]
    download_dir: Option<PathBuf>,
    /// Offer this file to the client when the session starts.
    #[arg(long)]
    offer_file: Option<PathBuf>,
    /// Fixed PIN for this run instead of the unattended settings.
    #[arg(long)]
    pin: Option<String>,
    /// auto, uinput, or none. Demo mode draws the pointer into the pattern.
    #[arg(long, default_value = "auto")]
    input: String,
    /// auto, screencopy, or portal. Screencopy shares every monitor with no
    /// picker; auto falls back to the portal picker when it is missing.
    #[arg(long, default_value = "auto")]
    capture: String,
    /// No desktop notification when a session starts.
    #[arg(long)]
    no_notify: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        None => serve(cli.serve),
        Some(Command::Serve(args)) => serve(args),
        Some(Command::Status { json }) => print_status(json),
        Some(Command::Pin { action }) => pin(action),
        Some(Command::Unattended { state }) => unattended(state),
        Some(Command::Disconnect) => disconnect(),
        Some(Command::ResetShare) => reset_share(),
    }
}

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let download_dir = match args.download_dir {
        Some(dir) => dir,
        None => default_download_dir()?,
    };
    let config = HostConfig {
        bind: args.bind,
        demo: args.demo,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        download_dir,
        offer_file: args.offer_file,
        pin: args.pin,
        settings_path: paths::settings_file(),
        state_path: paths::state_file(),
        restore_token: token_path(),
        input: InputMode::parse(&args.input).context("input mode")?,
        capture: CaptureMode::parse(&args.capture).context("capture mode")?,
        notify: !args.no_notify,
    };
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let runtime = tokio::runtime::Runtime::new().context("tokio runtime")?;
    runtime.block_on(async move {
        tokio::spawn(async move {
            wait_for_shutdown().await;
            shutdown.cancel();
        });
        run_host(config, None, cancel).await
    })
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn default_download_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Downloads").join("Omarchy Connect"))
}

fn token_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        omarchy_host::default_token_path()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn settings_path() -> anyhow::Result<PathBuf> {
    paths::settings_file().context("no config directory (HOME is not set)")
}

fn print_status(json: bool) -> anyhow::Result<()> {
    let state = paths::state_file().context("no runtime directory")?;
    let token = token_path();
    let report = status::report(&settings_path()?, &state, token.as_deref());
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{report}");
    }
    Ok(())
}

fn pin(action: PinAction) -> anyhow::Result<()> {
    let path = settings_path()?;
    let mut settings = Settings::load(&path)?;
    match action {
        PinAction::Set { pin } => {
            let pin = match pin {
                Some(pin) => pin,
                None => {
                    let mut line = String::new();
                    std::io::stdin()
                        .lock()
                        .read_line(&mut line)
                        .context("read PIN from stdin")?;
                    line
                }
            };
            let pin = omarchy_protocol::auth::validate_pin(&pin).map_err(anyhow::Error::msg)?;
            settings.pin = Some(pin.to_string());
            settings.unattended = true;
            settings.save(&path)?;
            println!("PIN saved. Unattended access is on.");
        }
        PinAction::Clear => {
            settings.pin = None;
            settings.unattended = false;
            settings.save(&path)?;
            println!("PIN removed. Unattended access is off.");
        }
    }
    Ok(())
}

fn unattended(state: OnOff) -> anyhow::Result<()> {
    let path = settings_path()?;
    let mut settings = Settings::load(&path)?;
    match state {
        OnOff::On => {
            if settings.pin.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("set a PIN first: omarchy-connect pin set");
            }
            settings.unattended = true;
        }
        OnOff::Off => settings.unattended = false,
    }
    settings.save(&path)?;
    println!(
        "Unattended access is {}.",
        if settings.unattended { "on" } else { "off" }
    );
    Ok(())
}

fn disconnect() -> anyhow::Result<()> {
    let state = paths::state_file().context("no runtime directory")?;
    let live = status::read_live(&state)
        .filter(|live| status::pid_alive(live.pid))
        .context("Omarchy Connect is not running")?;
    #[cfg(unix)]
    {
        let pid = i32::try_from(live.pid).context("pid")?;
        if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
            return Err(std::io::Error::last_os_error()).context("signal the host");
        }
    }
    #[cfg(not(unix))]
    let _ = live;
    println!("Disconnect sent.");
    Ok(())
}

fn reset_share() -> anyhow::Result<()> {
    let Some(path) = token_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Saved screen share forgotten. The picker appears on the next session."),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("No saved screen share.")
        }
        Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
    }
    Ok(())
}
