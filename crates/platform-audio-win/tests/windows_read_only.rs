#![cfg(windows)]

use platform_audio_win::{devices::*, Error};

/// Opens endpoint property/mix-format interfaces, but starts no stream.
#[test]
fn enumeration_and_notifications_are_read_only() {
    let mut monitor = watch_devices(8).unwrap();
    let endpoints = enumerate_devices().unwrap();
    let mut ids = std::collections::HashSet::new();
    for endpoint in endpoints {
        assert!(!endpoint.id.is_empty());
        assert!(ids.insert(endpoint.id));
        assert!(endpoint.mix_format.channels > 0);
        assert!(endpoint.mix_format.sample_rate > 0);
    }
    monitor.shutdown().unwrap();
}

/// Never calls SetMute, including cleanup. Missing capture devices are normal.
#[test]
fn capture_mute_read_and_unsubscribe() {
    let endpoints = enumerate_devices().unwrap();
    if let Some(endpoint) = endpoints
        .iter()
        .find(|endpoint| endpoint.flow == DeviceFlow::Capture)
    {
        let mut mute = CaptureMute::open(&endpoint.id, 8).unwrap();
        let _current_mute = mute.mute().unwrap();
        mute.shutdown().unwrap();
        assert!(matches!(mute.mute(), Err(Error::WorkerStopped)));
    }
    if let Some(endpoint) = endpoints
        .iter()
        .find(|endpoint| endpoint.flow == DeviceFlow::Render)
    {
        assert!(matches!(
            CaptureMute::open(&endpoint.id, 8),
            Err(Error::InvalidArgument(_))
        ));
    }
}

/// Does not create a player or issue any transport commands.
#[test]
fn smtc_sessions_and_subscriptions_are_read_only() {
    let manager = platform_audio_win::media::MediaManager::open().expect("SMTC manager");
    let snapshot = manager.snapshot().expect("read-only snapshot");
    for session in &snapshot.sessions {
        assert!(
            session.title.len() <= 256
                && session.artist.len() <= 256
                && session.app_id.len() <= 128
        );
    }
    if let Some(current) = snapshot.current {
        assert!(snapshot.sessions.iter().any(|s| s.token == current));
    }
    manager.shutdown();
}

#[test]
fn shared_backend_reads_devices_defaults_mute_and_media() {
    use audio_backend::{Backend, DeviceSelection, Direction};
    let backend = platform_audio_win::WindowsBackend::new().unwrap();
    let devices = backend.devices().unwrap();
    let _defaults = backend.defaults().unwrap();
    if let Some(capture) = devices
        .iter()
        .find(|device| device.direction == Direction::Capture)
    {
        let selection = DeviceSelection::Explicit(capture.id.clone());
        let first = backend.capture_mute(&selection).unwrap();
        let second = backend.capture_mute(&selection).unwrap();
        assert_eq!(first.endpoint, second.endpoint);
        assert_eq!(first.generation, second.generation);
    }
    for session in backend.media_sessions().unwrap() {
        assert!(session.title().len() <= 256);
        assert!(session.subtitle().len() <= 256);
        assert!(session.app().len() <= 128);
    }
}
