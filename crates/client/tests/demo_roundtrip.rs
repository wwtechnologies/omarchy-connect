use std::path::PathBuf;
use std::time::Duration;

use omarchy_client::{run_session, SessionConfig, SessionReport};
use omarchy_host::settings::Settings;
use omarchy_host::{run_host, CaptureMode, HostConfig, HostReady, InputMode};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const PIN: &str = "482913";

fn demo_config(download_dir: PathBuf) -> HostConfig {
    HostConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        demo: true,
        fps: 12,
        bitrate_kbps: 2000,
        download_dir,
        offer_file: None,
        pin: None,
        settings_path: None,
        state_path: None,
        restore_token: None,
        input: InputMode::None,
        capture: CaptureMode::Auto,
        notify: false,
    }
}

async fn start_host(config: HostConfig, cancel: CancellationToken) -> (HostReady, JoinHandle<()>) {
    let (ready_tx, ready_rx) = oneshot::channel();
    let host = tokio::spawn(async move {
        run_host(config, Some(ready_tx), cancel).await.unwrap();
    });
    let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("host ready timeout")
        .expect("host ready");
    (ready, host)
}

async fn connect(
    ready: &HostReady,
    pin: &str,
    download_dir: PathBuf,
    send_file: Option<PathBuf>,
) -> anyhow::Result<SessionReport> {
    let (_keep_sender, commands) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(45),
        run_session(
            SessionConfig {
                addr: ready.addr,
                pin: pin.into(),
                download_dir,
                send_file,
                stop_after_frames: Some(6),
            },
            commands,
            None,
        ),
    )
    .await
    .expect("session timeout")
}

#[tokio::test]
async fn demo_roundtrip_frames_and_files() {
    std::env::set_var("OMARCHY_FORCE_X264", "1");
    let dir = tempfile::tempdir().unwrap();
    let host_down = dir.path().join("host-in");
    let client_down = dir.path().join("client-in");
    std::fs::create_dir_all(&host_down).unwrap();
    std::fs::create_dir_all(&client_down).unwrap();
    let host_file = dir.path().join("from-host.bin");
    let client_file = dir.path().join("from-client.bin");
    let host_bytes: Vec<u8> = (0..80_000u32).map(|i| (i % 251) as u8).collect();
    let client_bytes: Vec<u8> = (0..90_000u32).map(|i| (i % 199) as u8).collect();
    std::fs::write(&host_file, &host_bytes).unwrap();
    std::fs::write(&client_file, &client_bytes).unwrap();

    let cancel = CancellationToken::new();
    let config = HostConfig {
        offer_file: Some(host_file),
        pin: Some(PIN.into()),
        ..demo_config(host_down.clone())
    };
    let (ready, host) = start_host(config, cancel.clone()).await;
    let report = connect(&ready, PIN, client_down, Some(client_file))
        .await
        .expect("session");

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), host)
        .await
        .expect("host shutdown")
        .expect("host task");

    assert_eq!(report.displays.len(), 2, "{report}");
    assert_eq!(report.displays[0].name, "eDP-1");
    assert_eq!(report.displays[0].width, 960);
    assert_eq!(report.displays[0].height, 540);
    assert_eq!(report.displays[0].scale_percent, 100);
    assert_eq!(report.displays[1].name, "HDMI-A-1");
    assert_eq!(report.displays[1].x, 960);
    assert_eq!(report.displays[1].scale_percent, 150);
    assert!(report.frames_decoded >= 6, "{report}");
    assert!(report.displays_with_motion.contains(&0), "{report}");
    assert!(report.displays_with_motion.contains(&1), "{report}");
    assert!(report.sent_ok, "{report}");
    let received = std::fs::read(report.received_path.expect("received path")).unwrap();
    assert_eq!(received, host_bytes);
    let saved = std::fs::read(host_down.join("from-client.bin")).unwrap();
    assert_eq!(saved, client_bytes);
}

#[tokio::test]
async fn unattended_settings_gate_the_pin() {
    std::env::set_var("OMARCHY_FORCE_X264", "1");
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    let state_path = dir.path().join("state.json");
    let cancel = CancellationToken::new();
    let config = HostConfig {
        settings_path: Some(settings_path.clone()),
        state_path: Some(state_path.clone()),
        ..demo_config(dir.path().join("host-in"))
    };
    let (ready, host) = start_host(config, cancel.clone()).await;
    let down = dir.path().join("client-in");

    let err = connect(&ready, PIN, down.clone(), None).await.unwrap_err();
    assert!(err.to_string().contains("Unattended access is off"), "{err}");

    Settings {
        unattended: true,
        pin: Some(PIN.into()),
    }
    .save(&settings_path)
    .unwrap();
    let err = connect(&ready, "111111", down.clone(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Wrong PIN"), "{err}");

    let report = connect(&ready, PIN, down.clone(), None).await.expect("session");
    assert!(report.frames_decoded >= 6, "{report}");
    assert!(state_path.exists());

    Settings {
        unattended: false,
        pin: Some(PIN.into()),
    }
    .save(&settings_path)
    .unwrap();
    let err = connect(&ready, PIN, down, None).await.unwrap_err();
    assert!(err.to_string().contains("Unattended access is off"), "{err}");

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), host)
        .await
        .expect("host shutdown")
        .expect("host task");
    assert!(!state_path.exists(), "state file is removed on shutdown");
}
