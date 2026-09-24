use std::time::Duration;

use omarchy_client::{parse_pin, run_session, SessionConfig};
use omarchy_host::{run_host, HostConfig, InputMode};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

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
    let (ready_tx, ready_rx) = oneshot::channel();
    let host_cancel = cancel.clone();
    let host = tokio::spawn(async move {
        run_host(
            HostConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                demo: true,
                fps: 12,
                bitrate_kbps: 2000,
                download_dir: host_down.clone(),
                offer_file: Some(host_file),
                pin_file: None,
                input: InputMode::None,
            },
            Some(ready_tx),
            host_cancel,
        )
        .await
        .unwrap();
        host_down
    });

    let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("host ready timeout")
        .expect("host ready");
    let (_keep_sender, commands) = tokio::sync::mpsc::unbounded_channel();
    let report = tokio::time::timeout(
        Duration::from_secs(45),
        run_session(
            SessionConfig {
                addr: ready.addr,
                pin: parse_pin(&ready.pin).unwrap(),
                download_dir: client_down,
                send_file: Some(client_file),
                stop_after_frames: Some(6),
            },
            commands,
            None,
        ),
    )
    .await
    .expect("session timeout")
    .expect("session");

    cancel.cancel();
    let host_down = tokio::time::timeout(Duration::from_secs(10), host)
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
