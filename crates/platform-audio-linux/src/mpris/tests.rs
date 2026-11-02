use super::*;
use std::{
    io::Read,
    sync::atomic::{AtomicI64, AtomicUsize},
    time::Duration,
};
use zbus::{
    object_server::SignalEmitter,
    zvariant::{ObjectPath, Value},
};
fn value(text: &str) -> OwnedValue {
    OwnedValue::try_from(Value::from(text)).unwrap()
}
fn fixture() -> Mpris {
    let mut data = Data::new();
    data.available = true;
    let now = Instant::now();
    let mut player = Player::new(":owned".into(), MediaSessionToken(1), now);
    let mut properties = Properties::new();
    properties.insert("CanControl".into(), true.into());
    properties.insert("CanPlay".into(), true.into());
    properties.insert("CanPause".into(), true.into());
    player.apply(PLAYER, properties, now);
    data.players.insert("owned".into(), player);
    Mpris {
        shared: Arc::new(Shared {
            data: Mutex::new(data),
            stop: AtomicBool::new(false),
        }),
        thread: None,
    }
}
fn request(id: u64) -> MediaCommandRequest {
    MediaCommandRequest {
        request_id: id,
        session: MediaSessionToken(1),
        expected_revision: 1,
        command: MediaCommand::Play,
    }
}
#[test]
fn reserved_completion_capacity_timeout_cancel_and_no_aba() {
    let mpris = fixture();
    for id in 1..=COMMAND_CAPACITY as u64 {
        mpris.command(request(id)).unwrap();
    }
    assert_eq!(mpris.command(request(100)), Err(BackendError::Busy));
    lock(&mpris.shared.data).expire(Instant::now() + Duration::from_secs(2));
    for id in 1..=COMMAND_CAPACITY as u64 {
        assert!(
            matches!(mpris.poll_completion(),Some(Event::MediaCommandCompleted {request_id,result:Err(BackendError::Timeout)}) if request_id==id)
        );
    }
    mpris.command(request(1)).unwrap();
    let old = lock(&mpris.shared.data).pending[0].ticket;
    mpris.cancel();
    assert!(matches!(
        mpris.poll_completion(),
        Some(Event::MediaCommandCompleted {
            result: Err(BackendError::Cancelled),
            ..
        })
    ));
    mpris.command(request(1)).unwrap();
    lock(&mpris.shared.data).complete(old, Ok(()));
    assert!(mpris.poll_completion().is_none());
    assert_eq!(lock(&mpris.shared.data).pending.len(), 1);
    lock(&mpris.shared.data).remove("owned");
    assert!(matches!(
        mpris.poll_completion(),
        Some(Event::MediaCommandCompleted {
            result: Err(BackendError::DeviceGone),
            ..
        })
    ));
    assert!(mpris.poll_completion().is_none());
}
#[test]
fn worker_exit_cancels_without_late_success() {
    let mpris = fixture();
    mpris.command(request(1)).unwrap();
    let ticket = lock(&mpris.shared.data).pending[0].ticket;
    drop(WorkerExit(mpris.shared.clone()));
    assert_eq!(
        mpris.availability(),
        Availability::Unavailable(BackendError::ServerUnavailable)
    );
    assert!(matches!(
        mpris.poll_completion(),
        Some(Event::MediaCommandCompleted {
            result: Err(BackendError::ServerUnavailable),
            ..
        })
    ));
    lock(&mpris.shared.data).complete(ticket, Ok(()));
    assert!(mpris.poll_completion().is_none());
    assert!(matches!(
        mpris.poll_changed(),
        Some(Event::MediaChanged { info: None, .. })
    ));
}

#[test]
fn revisions_selection_and_queue_overflow_are_explicit() {
    let mpris = fixture();
    let now = Instant::now();
    let mut data = lock(&mpris.shared.data);
    data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
    data.publish("owned", true, false, now);
    let mut second = Player::new(":second".into(), MediaSessionToken(2), now);
    second.state = PlaybackState::Playing;
    data.players.insert("second".into(), second);
    data.publish("second", true, false, now);
    drop(data);
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(2)));
    assert_eq!(mpris.command(request(1)), Err(BackendError::StaleRevision));
    let mut data = lock(&mpris.shared.data);
    data.players.get_mut("second").unwrap().state = PlaybackState::Paused;
    drop(data);
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    let mut data = lock(&mpris.shared.data);
    data.players.get_mut("owned").unwrap().state = PlaybackState::Unknown;
    for token in 10..=100 {
        data.emit(Event::MediaChanged {
            session: MediaSessionToken(token),
            info: None,
        });
    }
    assert!(data.changed.len() <= EVENT_CAPACITY);
    drop(data);
    assert!(mpris.current().unwrap().is_none());
    assert_eq!(mpris.poll_changed(), Some(Event::ResyncRequired));
}

#[test]
fn selected_live_player_remains_current_when_paused_or_stopped() {
    let mpris = fixture();
    let now = Instant::now();
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
        data.publish("owned", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    for (index, state) in [
        PlaybackState::Paused,
        PlaybackState::Stopped,
        PlaybackState::Playing,
    ]
    .into_iter()
    .enumerate()
    {
        {
            let mut data = lock(&mpris.shared.data);
            let player = data.players.get_mut("owned").unwrap();
            let was_playing = player.state == PlaybackState::Playing;
            player.state = state;
            data.publish("owned", true, was_playing, now);
        }
        assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
        let info = mpris.sessions().unwrap().into_iter().next().unwrap();
        assert_eq!(info.state, state);
        mpris
            .command(MediaCommandRequest {
                request_id: 100 + index as u64,
                session: info.session,
                expected_revision: info.revision,
                command: MediaCommand::Play,
            })
            .unwrap();
        mpris.cancel();
    }
}

#[test]
fn only_last_selected_player_is_a_fallback_and_new_playing_precedes_it() {
    let mpris = fixture();
    let now = Instant::now();
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Paused;
    }
    assert_eq!(mpris.current().unwrap(), None);
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
        data.publish("owned", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Paused;
        let mut second = Player::new(":second".into(), MediaSessionToken(2), now);
        second.state = PlaybackState::Playing;
        data.players.insert("second".into(), second);
        data.publish("second", true, false, now);
        // This player played and paused between consumer selections.
        data.players.get_mut("second").unwrap().state = PlaybackState::Paused;
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("second").unwrap().state = PlaybackState::Playing;
        data.publish("second", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(2)));
    lock(&mpris.shared.data)
        .players
        .get_mut("second")
        .unwrap()
        .state = PlaybackState::Stopped;
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(2)));
}

#[test]
fn selected_player_unknown_owner_loss_and_unavailability_do_not_revive_old_tokens() {
    let mpris = fixture();
    let now = Instant::now();
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
        data.publish("owned", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    lock(&mpris.shared.data)
        .players
        .get_mut("owned")
        .unwrap()
        .state = PlaybackState::Unknown;
    assert_eq!(mpris.current().unwrap(), None);
    lock(&mpris.shared.data)
        .players
        .get_mut("owned")
        .unwrap()
        .state = PlaybackState::Paused;
    assert_eq!(mpris.current().unwrap(), None);
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
        data.publish("owned", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(1)));
    {
        let mut data = lock(&mpris.shared.data);
        data.remove("owned");
        let mut replacement = Player::new(":replacement".into(), MediaSessionToken(2), now);
        replacement.state = PlaybackState::Paused;
        data.players.insert("owned".into(), replacement);
    }
    assert_eq!(mpris.current().unwrap(), None);
    assert_eq!(mpris.command(request(200)), Err(BackendError::DeviceGone));
    {
        let mut data = lock(&mpris.shared.data);
        data.players.get_mut("owned").unwrap().state = PlaybackState::Playing;
        data.publish("owned", true, false, now);
    }
    assert_eq!(mpris.current().unwrap(), Some(MediaSessionToken(2)));
    lock(&mpris.shared.data).unavailable();
    assert_eq!(mpris.current(), Err(BackendError::ServerUnavailable));
    lock(&mpris.shared.data).available = true;
    assert_eq!(mpris.current().unwrap(), None);
}
struct Root {
    app: String,
}
#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    fn identity(&self) -> &str {
        &self.app
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        &self.app
    }
}
#[derive(Default)]
struct Fake {
    state: AtomicUsize,
    commands: AtomicUsize,
    position: AtomicI64,
    slow_pause: AtomicBool,
    track: AtomicUsize,
}
struct FakePlayer {
    data: Arc<Fake>,
}
#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    async fn play(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.data.commands.fetch_add(1, Ordering::SeqCst);
        self.data.state.store(1, Ordering::SeqCst);
        self.playback_status_changed(&emitter).await.unwrap();
    }
    async fn pause(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.data.commands.fetch_add(1, Ordering::SeqCst);
        if self.data.slow_pause.load(Ordering::SeqCst) {
            futures_timer::Delay::new(Duration::from_millis(1500)).await;
        }
        self.data.state.store(0, Ordering::SeqCst);
        self.playback_status_changed(&emitter).await.unwrap();
    }
    async fn play_pause(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.data.commands.fetch_add(1, Ordering::SeqCst);
        self.data.state.fetch_xor(1, Ordering::SeqCst);
        self.playback_status_changed(&emitter).await.unwrap();
    }
    fn next(&self) {
        self.data.commands.fetch_add(1, Ordering::SeqCst);
    }
    fn previous(&self) {
        self.data.commands.fetch_add(1, Ordering::SeqCst);
    }
    async fn set_position(
        &self,
        track_id: ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let expected_track = if self.data.track.load(Ordering::SeqCst) == 0 {
            "/owned/track"
        } else {
            "/owned/next"
        };
        if track_id.as_str() != expected_track || !(0..=600_000_000).contains(&position) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "invalid track or position".into(),
            ));
        }
        self.data.position.store(position, Ordering::SeqCst);
        Self::seeked(&emitter, position).await?;
        Ok(())
    }
    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match self.data.state.load(Ordering::SeqCst) {
            0 => "Paused",
            1 => "Playing",
            _ => "NotAState",
        }
    }
    #[zbus(property)]
    fn metadata(&self) -> Properties {
        let mut metadata = Properties::new();
        metadata.insert("xesam:title".into(), value(&"🎵".repeat(100)));
        metadata.insert(
            "xesam:artist".into(),
            OwnedValue::try_from(Value::from(vec!["Owned", "Fake"])).unwrap(),
        );
        metadata.insert("mpris:length".into(), 600_000_000i64.into());
        metadata.insert(
            "mpris:trackid".into(),
            OwnedValue::try_from(Value::from(
                ObjectPath::try_from(if self.data.track.load(Ordering::SeqCst) == 0 {
                    "/owned/track"
                } else {
                    "/owned/next"
                })
                .unwrap(),
            ))
            .unwrap(),
        );
        metadata
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.data.position.load(Ordering::SeqCst)
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }
    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "owned MPRIS observation timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn own_info(mpris: &Mpris, app: &str) -> Option<MediaInfo> {
    mpris
        .sessions()
        .unwrap()
        .into_iter()
        .find(|info| info.app() == app)
}
fn completion(mpris: &Mpris, id: u64) -> Result<(), BackendError> {
    let mut result = None;
    wait_until(|| {
        if let Some(Event::MediaCommandCompleted {
            request_id,
            result: completed,
        }) = mpris.poll_completion()
        {
            assert_eq!(request_id, id);
            result = Some(completed);
        }
        result.is_some()
    });
    result.unwrap()
}
#[test]
fn live_owned_mpris_player_discovery_commands_seek_and_removal() {
    if std::env::var("BIFROST_MPRIS_TESTS").as_deref() != Ok("1") {
        eprintln!("skip owned MPRIS test: set BIFROST_MPRIS_TESTS=1");
        return;
    }
    let mut bytes = [0; 8];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    let app = format!("bifrost_test_{:016x}", u64::from_ne_bytes(bytes));
    let name = format!("{PREFIX}{app}");
    let data = Arc::new(Fake::default());
    let connection = futures::executor::block_on(async {
        zbus::connection::Builder::session()
            .unwrap()
            .name(name.as_str())
            .unwrap()
            .serve_at(PATH, Root { app: app.clone() })
            .unwrap()
            .serve_at(PATH, FakePlayer { data: data.clone() })
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let mpris = Mpris::start();
    assert_eq!(mpris.availability(), Availability::Available);
    wait_until(|| own_info(&mpris, &app).is_some());
    let info = own_info(&mpris, &app).unwrap();
    let token = info.session;
    assert_eq!(info.title().len(), 256);
    assert!(info.subtitle() == "Owned, Fake");
    assert_eq!(info.duration_ms, Some(600000));
    assert_eq!(info.state, PlaybackState::Paused);
    mpris
        .command(MediaCommandRequest {
            request_id: 1,
            session: token,
            expected_revision: info.revision,
            command: MediaCommand::Play,
        })
        .unwrap();
    assert_eq!(completion(&mpris, 1), Ok(()));
    wait_until(|| own_info(&mpris, &app).unwrap().state == PlaybackState::Playing);
    assert_eq!(data.commands.load(Ordering::SeqCst), 1);
    assert_eq!(mpris.current().unwrap(), Some(token));
    data.position.store(3_000_000, Ordering::SeqCst);
    futures::executor::block_on(async {
        let interface = connection
            .object_server()
            .interface::<_, FakePlayer>(PATH)
            .await
            .unwrap();
        FakePlayer::seeked(interface.signal_emitter(), 3_000_000)
            .await
            .unwrap();
    });
    wait_until(|| {
        own_info(&mpris, &app)
            .unwrap()
            .position_ms
            .is_some_and(|position| (3000..4000).contains(&position))
    });
    let changed = own_info(&mpris, &app).unwrap();
    assert!(changed.commands.seek);
    mpris
        .command(MediaCommandRequest {
            request_id: 20,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::SeekTo {
                position_ms: 123_456,
            },
        })
        .unwrap();
    assert_eq!(completion(&mpris, 20), Ok(()));
    assert_eq!(data.position.load(Ordering::SeqCst), 123_456_000);
    wait_until(|| {
        own_info(&mpris, &app)
            .unwrap()
            .position_ms
            .is_some_and(|position| (123_456..124_456).contains(&position))
    });
    let changed = own_info(&mpris, &app).unwrap();
    data.track.store(1, Ordering::SeqCst);
    futures::executor::block_on(async {
        let interface = connection
            .object_server()
            .interface::<_, FakePlayer>(PATH)
            .await
            .unwrap();
        let player = interface.get().await;
        player
            .metadata_changed(interface.signal_emitter())
            .await
            .unwrap();
        player
            .position_changed(interface.signal_emitter())
            .await
            .unwrap();
    });
    wait_until(|| {
        own_info(&mpris, &app).unwrap().revision > changed.revision
            && own_info(&mpris, &app).unwrap().commands.seek
    });
    assert_eq!(
        mpris.command(MediaCommandRequest {
            request_id: 21,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::SeekTo { position_ms: 1 },
        }),
        Err(BackendError::StaleRevision)
    );
    let changed = own_info(&mpris, &app).unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 22,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::SeekTo { position_ms: 0 },
        })
        .unwrap();
    assert_eq!(completion(&mpris, 22), Ok(()));
    assert_eq!(data.position.load(Ordering::SeqCst), 0);
    wait_until(|| own_info(&mpris, &app).unwrap().revision > changed.revision);
    let changed = own_info(&mpris, &app).unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 23,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::Pause,
        })
        .unwrap();
    assert_eq!(completion(&mpris, 23), Ok(()));
    wait_until(|| own_info(&mpris, &app).unwrap().state == PlaybackState::Paused);
    assert_eq!(mpris.current().unwrap(), Some(token));
    let paused = own_info(&mpris, &app).unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 24,
            session: token,
            expected_revision: paused.revision,
            command: MediaCommand::SeekTo { position_ms: 42 },
        })
        .unwrap();
    assert_eq!(completion(&mpris, 24), Ok(()));
    assert_eq!(data.position.load(Ordering::SeqCst), 42_000);
    wait_until(|| own_info(&mpris, &app).unwrap().position_ms == Some(42));
    let paused = own_info(&mpris, &app).unwrap();
    assert_eq!(paused.state, PlaybackState::Paused);
    mpris
        .command(MediaCommandRequest {
            request_id: 25,
            session: token,
            expected_revision: paused.revision,
            command: MediaCommand::Play,
        })
        .unwrap();
    assert_eq!(completion(&mpris, 25), Ok(()));
    wait_until(|| own_info(&mpris, &app).unwrap().state == PlaybackState::Playing);
    let changed = own_info(&mpris, &app).unwrap();
    assert!(changed.revision > info.revision);
    assert_eq!(
        mpris.command(MediaCommandRequest {
            request_id: 2,
            session: token,
            expected_revision: info.revision,
            command: MediaCommand::Next
        }),
        Err(BackendError::StaleRevision)
    );
    data.slow_pause.store(true, Ordering::SeqCst);
    mpris
        .command(MediaCommandRequest {
            request_id: 3,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::Pause,
        })
        .unwrap();
    assert_eq!(completion(&mpris, 3), Err(BackendError::Timeout));
    thread::sleep(Duration::from_millis(650));
    assert!(mpris.poll_completion().is_none());
    assert_eq!(data.commands.load(Ordering::SeqCst), 4);
    let current = own_info(&mpris, &app).unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 10,
            session: token,
            expected_revision: current.revision,
            command: MediaCommand::Play,
        })
        .unwrap();
    assert_eq!(completion(&mpris, 10), Ok(()));
    wait_until(|| own_info(&mpris, &app).unwrap().state == PlaybackState::Playing);
    let current = own_info(&mpris, &app).unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 11,
            session: token,
            expected_revision: current.revision,
            command: MediaCommand::Pause,
        })
        .unwrap();
    mpris
        .command(MediaCommandRequest {
            request_id: 12,
            session: token,
            expected_revision: current.revision,
            command: MediaCommand::Next,
        })
        .unwrap();
    wait_until(|| data.commands.load(Ordering::SeqCst) == 6);
    mpris.cancel();
    assert_eq!(completion(&mpris, 11), Err(BackendError::Cancelled));
    assert_eq!(completion(&mpris, 12), Err(BackendError::Cancelled));
    thread::sleep(Duration::from_millis(1600));
    assert_eq!(data.commands.load(Ordering::SeqCst), 6);
    assert!(mpris.poll_completion().is_none());
    futures::executor::block_on(connection.release_name(name.as_str())).unwrap();
    wait_until(|| own_info(&mpris, &app).is_none());
    assert_eq!(
        mpris.command(MediaCommandRequest {
            request_id: 4,
            session: token,
            expected_revision: changed.revision,
            command: MediaCommand::Next
        }),
        Err(BackendError::DeviceGone)
    );
    let mut removed = false;
    while let Some(event) = mpris.poll_changed() {
        removed |= matches!(event,Event::MediaChanged {session,info:None} if session==token);
    }
    assert!(removed);
    futures::executor::block_on(connection.request_name(name.as_str())).unwrap();
    wait_until(|| own_info(&mpris, &app).is_some_and(|info| info.session != token));
    futures::executor::block_on(connection.release_name(name.as_str())).unwrap();
    wait_until(|| own_info(&mpris, &app).is_none());
    drop(mpris);
    let proxy = futures::executor::block_on(bus_proxy(&connection)).unwrap();
    let names: Vec<String> = futures::executor::block_on(proxy.call("ListNames", &())).unwrap();
    assert!(!names.iter().any(|candidate| candidate == &name));
}
