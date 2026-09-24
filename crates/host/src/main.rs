use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use omarchy_host::{run_host, HostConfig, InputMode};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "host", about = "Omarchy Connect host")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:47921")]
    bind: SocketAddr,
    /// Two synthetic monitors instead of the xdg-desktop-portal capture.
    #[arg(long)]
    demo: bool,
    #[arg(long, default_value_t = 15)]
    fps: u32,
    #[arg(long, default_value_t = 4000)]
    bitrate_kbps: u32,
    #[arg(long, default_value = "downloads")]
    download_dir: PathBuf,
    /// Offer this file to the client when the session starts.
    #[arg(long)]
    offer_file: Option<PathBuf>,
    /// Write the session pin (SHA-256 of the ephemeral certificate) here.
    #[arg(long)]
    pin_file: Option<PathBuf>,
    /// auto, portal, uinput, or none. Demo mode draws the pointer into the pattern.
    #[arg(long, default_value = "auto")]
    input: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let config = HostConfig {
        bind: args.bind,
        demo: args.demo,
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        download_dir: args.download_dir,
        offer_file: args.offer_file,
        pin_file: args.pin_file,
        input: InputMode::parse(&args.input).context("input mode")?,
    };
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let runtime = tokio::runtime::Runtime::new().context("tokio runtime")?;
    runtime.block_on(async move {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        });
        run_host(config, None, cancel).await
    })
}
