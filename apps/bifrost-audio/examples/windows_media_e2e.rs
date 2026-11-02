//! Opt-in own-player checks; never command an existing user's player.
//! `--owned-seek-test` uses zero PCM and an own-player volume of zero.
//! The separate `--owned-test` mode is an audible process-excluding capture check.
//! Neither mode changes endpoint settings, installed Bifrost, or system input.
#[cfg(windows)]
mod native {
    use audio_backend::{
        Backend, BackendError, Event as AudioEvent, MediaCommand, MediaCommandRequest, MediaInfo,
    };
    use ipc::{
        audio_worker::{AudioLink, LaunchToken},
        peer::SameUser,
        transport::{self, Endpoint},
    };
    use platform_audio_win::WindowsBackend;
    use protocol::audio_worker::{self as w, audio_frame::Msg};
    use ring::rand::{SecureRandom, SystemRandom};
    use std::{
        error::Error,
        io::{BufRead, BufReader, Write},
        net::UdpSocket,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use windows::{
        core::{IUnknown, Interface, HSTRING, PCWSTR},
        Foundation::{TimeSpan, TypedEventHandler},
        Media::{
            Control::{
                GlobalSystemMediaTransportControlsSession as NativeSession,
                GlobalSystemMediaTransportControlsSessionManager as NativeManager,
            },
            Core::MediaSource,
            MediaPlaybackStatus, MediaPlaybackType,
            Playback::{MediaPlaybackState, MediaPlayer},
            PlaybackPositionChangeRequestedEventArgs, SystemMediaTransportControls,
            SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
            SystemMediaTransportControlsTimelineProperties,
        },
        Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
        Win32::{
            System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
            UI::Shell::SetCurrentProcessExplicitAppUserModelID,
        },
    };

    type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
    const SESSION: [u8; 16] = [39; 16];
    const LIMIT: Duration = Duration::from_secs(25);

    struct Apartment;
    impl Apartment {
        fn open() -> Result<Self> {
            unsafe {
                RoInitialize(RO_INIT_MULTITHREADED)?;
            }
            Ok(Self)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                RoUninitialize();
            }
        }
    }
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn child(mode: &str) -> Result<OwnedChild> {
        Ok(OwnedChild(
            Command::new(std::env::current_exe()?)
                .args(["--owned-test", mode])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        ))
    }
    fn input(child: &mut OwnedChild, line: &str) -> Result<()> {
        let pipe = child.0.stdin.as_mut().ok_or("missing private stdin")?;
        writeln!(pipe, "{line}")?;
        pipe.flush()?;
        Ok(())
    }
    fn lines(child: &mut OwnedChild) -> Result<tokio::sync::mpsc::Receiver<String>> {
        let output = child.0.stdout.take().ok_or("missing private stdout")?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }
    fn random<const N: usize>() -> Result<[u8; N]> {
        let mut value = [0; N];
        SystemRandom::new()
            .fill(&mut value)
            .map_err(|_| "random generation failed")?;
        Ok(value)
    }
    fn nonce() -> Result<String> {
        Ok(random::<16>()?.iter().map(|b| format!("{b:02x}")).collect())
    }

    // One second of 440 Hz PCM, looped by this helper alone; no temporary files.
    fn wave() -> Vec<u8> {
        const RATE: u32 = 48_000;
        let bytes = RATE * 2;
        let mut wav = Vec::with_capacity(bytes as usize + 44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(bytes + 36).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes.to_le_bytes());
        for i in 0..RATE {
            let sample = ((f64::from(i) * 440.0 * std::f64::consts::TAU / f64::from(RATE)).sin()
                * 1600.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    fn helper() -> Result<()> {
        let mut nonce = String::new();
        std::io::stdin().read_line(&mut nonce)?;
        let nonce = nonce.trim();
        if nonce.len() != 32 || !nonce.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid private helper identity".into());
        }
        let app = format!("BifrostTest.{nonce}");
        let title = format!("Bifrost test tone {nonce}");
        let wide: Vec<_> = app.encode_utf16().chain(Some(0)).collect();
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr()))?;
        }
        let _apartment = Apartment::open()?;
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(&wave())?;
        writer.StoreAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        let source = MediaSource::CreateFromStream(&stream, &HSTRING::from("audio/wav"))?;
        let player = MediaPlayer::new()?;
        player.SetIsLoopingEnabled(true)?;
        player.CommandManager()?.SetIsEnabled(false)?;
        player.SetSource(&source)?;
        let controls = player.SystemMediaTransportControls()?;
        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        let display = controls.DisplayUpdater()?;
        display.SetType(MediaPlaybackType::Music)?;
        display.MusicProperties()?.SetTitle(&HSTRING::from(title))?;
        display.Update()?;
        let (events, received) = mpsc::sync_channel(16);
        let owned_player = player.clone();
        let owned_controls = controls.clone();
        let registration = controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let args = args.as_ref().ok_or_else(|| {
                windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))
            })?;
            let command = args.Button()?;
            let name = if command == SystemMediaTransportControlsButton::Play {
                owned_player.Play()?;
                owned_controls.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;
                "play"
            } else if command == SystemMediaTransportControlsButton::Pause {
                owned_player.Pause()?;
                owned_controls.SetPlaybackStatus(MediaPlaybackStatus::Paused)?;
                "pause"
            } else if command == SystemMediaTransportControlsButton::Next {
                // The helper's only track repeats: Next restarts that owned track.
                owned_player
                    .PlaybackSession()?
                    .SetPosition(TimeSpan { Duration: 0 })?;
                "next"
            } else {
                return Ok(());
            };
            let _ = events.try_send(name);
            Ok(())
        }))?;
        player.Play()?;
        controls.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;
        println!("READY");
        std::io::stdout().flush()?;
        let (quit_tx, quit_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let _ = quit_tx.send(());
        });
        let until = std::time::Instant::now() + LIMIT;
        while std::time::Instant::now() < until && quit_rx.try_recv().is_err() {
            if let Ok(event) = received.recv_timeout(Duration::from_millis(20)) {
                println!("ACK {event}");
                std::io::stdout().flush()?;
            }
        }
        controls.RemoveButtonPressed(registration)?;
        controls.SetIsEnabled(false)?;
        player.Close()?;
        source.Close()?;
        Ok(())
    }

    const SEEK_START_MS: i64 = 60_000;
    const SEEK_DURATION_MS: i64 = 60_000;
    const SEEK_TARGET_MS: i64 = 20_000;

    #[derive(Clone, Copy)]
    enum SeekHelperEvent {
        Playback(&'static str),
        Seek { requested: i64, target: i64 },
    }

    fn invalid_seek() -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))
    }

    fn provider_seek_offset(requested: i64, start: i64, duration: i64) -> Option<i64> {
        let target = requested.checked_sub(start)?;
        (duration > 0 && target >= 0 && target <= duration).then_some(target)
    }

    fn silent_wave() -> Vec<u8> {
        const RATE: u32 = 48_000;
        let bytes = RATE * 2 * (SEEK_DURATION_MS as u32 / 1000);
        let mut wav = Vec::with_capacity(bytes as usize + 44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(bytes + 36).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes.to_le_bytes());
        wav.resize(wav.len() + bytes as usize, 0);
        wav
    }

    fn publish_seek_timeline(
        controls: &SystemMediaTransportControls,
        start: i64,
        duration: i64,
        position: i64,
    ) -> windows::core::Result<()> {
        if duration <= 0 || !(0..=duration).contains(&position) {
            return Err(invalid_seek());
        }
        let end = start.checked_add(duration).ok_or_else(invalid_seek)?;
        let position = start.checked_add(position).ok_or_else(invalid_seek)?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
        timeline.SetStartTime(TimeSpan { Duration: start })?;
        timeline.SetMinSeekTime(TimeSpan { Duration: start })?;
        timeline.SetPosition(TimeSpan { Duration: position })?;
        timeline.SetMaxSeekTime(TimeSpan { Duration: end })?;
        timeline.SetEndTime(TimeSpan { Duration: end })?;
        controls.UpdateTimelineProperties(&timeline)
    }

    fn seek_helper(echo: bool) -> Result<()> {
        let mut nonce = String::new();
        std::io::stdin().read_line(&mut nonce)?;
        let nonce = nonce.trim();
        if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid private helper identity".into());
        }
        let app = format!("BifrostSeekProbe.{nonce}");
        let title = format!("Bifrost seek probe {nonce}");
        let wide: Vec<_> = app.encode_utf16().chain(Some(0)).collect();
        unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr()))? };
        let _apartment = Apartment::open()?;
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(&silent_wave())?;
        writer.StoreAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        let source = MediaSource::CreateFromStream(&stream, &HSTRING::from("audio/wav"))?;
        let player = MediaPlayer::new()?;
        player.SetIsLoopingEnabled(true)?;
        player.CommandManager()?.SetIsEnabled(false)?;
        player.SetVolume(0.0)?;
        player.SetSource(&source)?;
        let controls = player.SystemMediaTransportControls()?;
        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        let display = controls.DisplayUpdater()?;
        display.SetType(MediaPlaybackType::Music)?;
        display.MusicProperties()?.SetTitle(&HSTRING::from(title))?;
        display.Update()?;
        let start = SEEK_START_MS
            .checked_mul(10_000)
            .ok_or("fixture start overflow")?;
        let duration = SEEK_DURATION_MS
            .checked_mul(10_000)
            .ok_or("fixture duration overflow")?;
        publish_seek_timeline(&controls, start, duration, 0)?;
        let (events, received) = mpsc::sync_channel::<SeekHelperEvent>(16);
        let button_player = player.clone();
        let button_controls = controls.clone();
        let button_events = events.clone();
        let button_registration = controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let args = args.as_ref().ok_or_else(invalid_seek)?;
            let name = if args.Button()? == SystemMediaTransportControlsButton::Pause {
                button_player.Pause()?;
                button_controls.SetPlaybackStatus(MediaPlaybackStatus::Paused)?;
                "pause"
            } else if args.Button()? == SystemMediaTransportControlsButton::Play {
                button_player.Play()?;
                button_controls.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;
                "play"
            } else {
                return Err(invalid_seek());
            };
            let _ = button_events.try_send(SeekHelperEvent::Playback(name));
            Ok(())
        }))?;
        let seek_player = player.clone();
        let seek_events = events;
        let seek_registration =
            controls.PlaybackPositionChangeRequested(&TypedEventHandler::<
                SystemMediaTransportControls,
                PlaybackPositionChangeRequestedEventArgs,
            >::new(move |_, args| {
                let requested = args
                    .as_ref()
                    .ok_or_else(invalid_seek)?
                    .RequestedPlaybackPosition()?
                    .Duration;
                let target =
                    provider_seek_offset(requested, start, duration).ok_or_else(invalid_seek)?;
                // Admission is not position proof. Native SetPosition is asynchronous.
                if !echo {
                    seek_player
                        .PlaybackSession()?
                        .SetPosition(TimeSpan { Duration: target })?;
                }
                let _ = seek_events.try_send(SeekHelperEvent::Seek { requested, target });
                Ok(())
            }))?;
        player.Play()?;
        if echo {
            player.Pause()?;
            controls.SetPlaybackStatus(MediaPlaybackStatus::Paused)?;
        } else {
            controls.SetPlaybackStatus(MediaPlaybackStatus::Playing)?;
        }
        let clock = Instant::now();
        println!(
            "SEEK_READY {} {}",
            std::process::id(),
            clock.elapsed().as_micros()
        );
        std::io::stdout().flush()?;
        let (commands, command_rx) = mpsc::sync_channel(8);
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let quit = line == "quit";
                if commands.send(line).is_err() || quit {
                    break;
                }
            }
        });
        let until = Instant::now() + LIMIT;
        let mut publication_target = None;
        while Instant::now() < until {
            while let Ok(event) = received.try_recv() {
                match event {
                    SeekHelperEvent::Playback(name) => println!("PLAYBACK_ACK {name}"),
                    SeekHelperEvent::Seek { requested, target } => {
                        publication_target = Some(target);
                        println!("SEEK_REQUEST {requested} {}", clock.elapsed().as_micros());
                    }
                }
                std::io::stdout().flush()?;
            }
            match command_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(line) if line == "quit" => break,
                Ok(line) => {
                    let sequence = line
                        .strip_prefix("OBSERVE ")
                        .ok_or("unknown helper command")?
                        .parse::<u64>()?;
                    let playback = player.PlaybackSession()?;
                    let actual = playback.Position()?.Duration;
                    let state = playback.PlaybackState()?.0;
                    // Publish once, only after a separate getter observes actual native readiness.
                    let publish_ready = publication_target
                        .is_some_and(|target| actual.abs_diff(target) <= 1_000_000);
                    if publish_ready {
                        publish_seek_timeline(&controls, start, duration, actual)?;
                        publication_target = None;
                    }
                    println!(
                        "OBSERVE {sequence} {} {actual} {state} {}",
                        clock.elapsed().as_micros(),
                        u8::from(publish_ready)
                    );
                    std::io::stdout().flush()?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        controls.RemoveButtonPressed(button_registration)?;
        controls.RemovePlaybackPositionChangeRequested(seek_registration)?;
        controls.SetIsEnabled(false)?;
        player.Pause()?;
        player.Close()?;
        source.Close()?;
        Ok(())
    }

    fn sync_lines(child: &mut OwnedChild) -> Result<std::sync::mpsc::Receiver<String>> {
        let output = child.0.stdout.take().ok_or("missing helper stdout")?;
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(rx)
    }

    fn sync_line(lines: &std::sync::mpsc::Receiver<String>, timeout: Duration) -> Result<String> {
        lines
            .recv_timeout(timeout)
            .map_err(|_| "helper line timeout".into())
    }

    #[derive(Debug, Clone, Copy)]
    struct PlayerObservation {
        at_us: u128,
        position_ticks: i64,
        state: i32,
        published: bool,
    }

    fn observe_player(
        helper: &mut OwnedChild,
        lines: &std::sync::mpsc::Receiver<String>,
        sequence: &mut u64,
    ) -> Result<PlayerObservation> {
        *sequence += 1;
        input(helper, &format!("OBSERVE {sequence}"))?;
        let line = sync_line(lines, Duration::from_secs(3))?;
        let mut fields = line.split_whitespace();
        if fields.next() != Some("OBSERVE")
            || fields
                .next()
                .ok_or("missing observation sequence")?
                .parse::<u64>()?
                != *sequence
        {
            return Err("unexpected own observation response".into());
        }
        let observation = PlayerObservation {
            at_us: fields
                .next()
                .ok_or("missing observation timestamp")?
                .parse()?,
            position_ticks: fields
                .next()
                .ok_or("missing actual player position")?
                .parse()?,
            state: fields
                .next()
                .ok_or("missing actual player state")?
                .parse()?,
            published: match fields.next() {
                Some("0") => false,
                Some("1") => true,
                _ => return Err("invalid publication observation".into()),
            },
        };
        if fields.next().is_some() {
            return Err("trailing observation fields".into());
        }
        println!("PLAYER_OBSERVATION {sequence} {observation:?}");
        Ok(observation)
    }

    fn observe_state(
        helper: &mut OwnedChild,
        lines: &std::sync::mpsc::Receiver<String>,
        sequence: &mut u64,
        expected: MediaPlaybackState,
    ) -> Result<PlayerObservation> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let observation = observe_player(helper, lines, sequence)?;
            if observation.state == expected.0 {
                return Ok(observation);
            }
            if Instant::now() >= until {
                return Err("actual native player state readiness timeout".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SeekProofError {
        Overflow,
        RequestCoordinate,
        PlayerPosition,
        PublishedPosition,
    }

    fn verify_seek(
        start: i64,
        requested: i64,
        target: i64,
        player: i64,
        published: i64,
    ) -> std::result::Result<(), SeekProofError> {
        let expected = start.checked_add(target).ok_or(SeekProofError::Overflow)?;
        if requested != expected {
            return Err(SeekProofError::RequestCoordinate);
        }
        if player.abs_diff(target) > 1_000_000 {
            return Err(SeekProofError::PlayerPosition);
        }
        if published.abs_diff(expected) > 1_000_000 {
            return Err(SeekProofError::PublishedPosition);
        }
        Ok(())
    }

    fn raw_current(manager: &NativeManager) -> Result<Option<NativeSession>> {
        let mut pointer = std::ptr::null_mut();
        unsafe {
            (Interface::vtable(manager).GetCurrentSession)(
                Interface::as_raw(manager),
                &mut pointer,
            )
            .ok()?;
        }
        Ok(if pointer.is_null() {
            None
        } else {
            Some(unsafe { NativeSession::from_raw(pointer) })
        })
    }

    fn is_owned(session: &NativeSession, app: &str, title: &str) -> Result<bool> {
        Ok(session.SourceAppUserModelId()? == app
            && session.TryGetMediaPropertiesAsync()?.join()?.Title()? == title)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum OwnAdmission {
        Current(audio_backend::MediaSessionToken),
        PrimitiveOnly(audio_backend::MediaSessionToken),
    }

    fn admit_owned(
        candidates: &[MediaInfo],
        current: Option<audio_backend::MediaSessionToken>,
    ) -> Option<OwnAdmission> {
        if candidates
            .iter()
            .any(|info| info.session.0 == 0 || info.revision == 0 || info.revision == u64::MAX)
        {
            return None;
        }
        if let Some(token) =
            current.filter(|token| candidates.iter().any(|info| info.session == *token))
        {
            return Some(OwnAdmission::Current(token));
        }
        match candidates {
            [only] => Some(OwnAdmission::PrimitiveOnly(only.session)),
            _ => None,
        }
    }

    fn find_owned(
        backend: &mut WindowsBackend,
        manager: &NativeManager,
        app: &str,
        title: &str,
        phase: &str,
    ) -> Result<(MediaInfo, NativeSession)> {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let mut candidates = backend.media_sessions()?;
            candidates.retain(|info| info.app() == app && info.title() == title);
            let selected = backend.current_media_session()?;
            let native_current = backend.native_current_media_session()?;
            let mut current = raw_current(manager)?;
            let current_owned = current
                .as_ref()
                .map(|session| is_owned(session, app, title))
                .transpose()?
                .unwrap_or(false);
            let current_identity = current
                .as_ref()
                .map(|session| session.cast::<IUnknown>())
                .transpose()?;
            let mut raw_owned = Vec::new();
            let mut current_in_catalog = false;
            for session in manager.GetSessions()? {
                let identity: IUnknown = session.cast()?;
                current_in_catalog |= current_identity.as_ref() == Some(&identity);
                if is_owned(&session, app, title)? {
                    raw_owned.push(session);
                }
            }
            let current_state = if current_owned {
                "own"
            } else if current.is_some() {
                "other"
            } else {
                "none"
            };
            println!("OWN_VIEWS {phase} backend_count={} raw_catalog_count={} os_current={current_state} current_in_catalog={current_in_catalog} backend_selected={selected:?} native_current={native_current:?}",
                candidates.len(), raw_owned.len());
            for info in &candidates {
                println!(
                    "OWN_TOKEN {phase} {:?} revision={}",
                    info.session, info.revision
                );
            }
            let admission = admit_owned(&candidates, native_current);
            if let Some(admission) = admission {
                let token = match admission {
                    OwnAdmission::Current(token) | OwnAdmission::PrimitiveOnly(token) => token,
                };
                let raw = if current_owned {
                    current.take()
                } else if raw_owned.len() == 1 {
                    raw_owned.pop()
                } else {
                    None
                };
                let raw =
                    raw.ok_or("owned raw native view is absent/ambiguous; no command sent")?;
                println!("OWN_ADMISSION {phase} {admission:?}");
                let info = candidates
                    .into_iter()
                    .find(|info| info.session == token)
                    .ok_or("owned token disappeared before admission")?;
                return Ok((info, raw));
            }
            if !candidates.is_empty() {
                return Err(
                    "multiple owned native views without an exact owned OS choice; no command sent"
                        .into(),
                );
            }
            if Instant::now() >= until {
                return Err(
                    "owned catalog session missing after bounded readiness; no command sent".into(),
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn await_backend_command(
        backend: &mut WindowsBackend,
        request: MediaCommandRequest,
    ) -> std::result::Result<std::result::Result<(), BackendError>, BackendError> {
        let id = request.request_id;
        backend.media_command(request)?;
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            while let Some(event) = backend.poll_event() {
                if let AudioEvent::MediaCommandCompleted { request_id, result } = event {
                    if request_id == id {
                        return Ok(result);
                    }
                }
            }
            if Instant::now() >= until {
                return Err(BackendError::Timeout);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn require_backend_ok(
        result: std::result::Result<std::result::Result<(), BackendError>, BackendError>,
    ) -> Result<()> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("backend command rejected: {error:?}").into()),
            Err(error) => Err(format!("backend command failed: {error:?}").into()),
        }
    }

    fn wait_backend_gone(backend: &mut WindowsBackend, app: &str, title: &str) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let present = backend
                .media_sessions()?
                .into_iter()
                .any(|info| info.app() == app && info.title() == title);
            if !present {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err("owned backend session did not disappear".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn owned_seek_pair(
        backend: &mut WindowsBackend,
        manager: &NativeManager,
        app: &str,
        title: &str,
        echo: bool,
        check_removal: bool,
    ) -> Result<()> {
        let mut helper = child(if echo {
            "--seek-echo-helper"
        } else {
            "--seek-helper"
        })?;
        let lines = sync_lines(&mut helper)?;
        input(&mut helper, app.rsplit('.').next().ok_or("identity")?)?;
        let ready = sync_line(&lines, LIMIT)?;
        let mut ready_fields = ready.split_whitespace();
        if ready_fields.next() != Some("SEEK_READY")
            || ready_fields
                .next()
                .ok_or("missing helper PID")?
                .parse::<u32>()?
                != helper.0.id()
        {
            return Err("owned helper process identity mismatch".into());
        }
        let ready_at_us = ready_fields
            .next()
            .ok_or("missing helper initialization time")?
            .parse::<u128>()?;
        if ready_fields.next().is_some() {
            return Err("invalid helper initialization response".into());
        }
        println!(
            "OWN_HELPER_READY pid={} echo={echo} at_us={ready_at_us}",
            helper.0.id()
        );
        let mut sequence = 0;
        let baseline = observe_state(
            &mut helper,
            &lines,
            &mut sequence,
            if echo {
                MediaPlaybackState::Paused
            } else {
                MediaPlaybackState::Playing
            },
        )?;
        let (info, raw) = find_owned(backend, manager, app, title, "before-seek")?;
        if info.seek_range().is_none() {
            return Err("backend did not advertise a valid owned native seek range".into());
        }
        let timeline = raw.GetTimelineProperties()?;
        let start = timeline.StartTime()?.Duration;
        let end = timeline.EndTime()?.Duration;
        let target = SEEK_TARGET_MS
            .checked_mul(10_000)
            .ok_or("target overflow")?;
        let expected_start = SEEK_START_MS.checked_mul(10_000).ok_or("start overflow")?;
        let expected_end = expected_start
            .checked_add(
                SEEK_DURATION_MS
                    .checked_mul(10_000)
                    .ok_or("duration overflow")?,
            )
            .ok_or("end overflow")?;
        if start != expected_start || start <= 0 || end != expected_end {
            return Err(
                "owned provider's declared nonzero published timeline was not observed".into(),
            );
        }
        println!(
            "PUBLISHED_BASELINE start={start} end={end} position={}",
            timeline.Position()?.Duration
        );
        require_backend_ok(await_backend_command(
            backend,
            MediaCommandRequest {
                request_id: 31,
                session: info.session,
                expected_revision: info.revision,
                command: MediaCommand::SeekTo {
                    position_ms: SEEK_TARGET_MS as u64,
                },
            },
        ))?;
        let request = sync_line(&lines, Duration::from_secs(3))?;
        let mut request_fields = request.split_whitespace();
        if request_fields.next() != Some("SEEK_REQUEST") {
            return Err("native seek request observation missing".into());
        }
        let requested = request_fields
            .next()
            .ok_or("missing native request coordinate")?
            .parse::<i64>()?;
        let request_at_us = request_fields
            .next()
            .ok_or("missing native request timestamp")?
            .parse::<u128>()?;
        if request_fields.next().is_some() {
            return Err("invalid native request observation".into());
        }
        println!("RAW_NATIVE_REQUEST requested={requested} at_us={request_at_us}");
        let until = Instant::now() + Duration::from_secs(2);
        let actual = loop {
            let observation = observe_player(&mut helper, &lines, &mut sequence)?;
            if observation.at_us < request_at_us || observation.at_us < baseline.at_us {
                return Err("stale actual-player observation".into());
            }
            if echo
                || (observation.position_ticks.abs_diff(target) <= 1_000_000
                    && observation.published)
            {
                break observation;
            }
            if Instant::now() >= until {
                return Err("actual native seek readiness timeout".into());
            }
            thread::sleep(Duration::from_millis(20));
        };
        if echo {
            let published = raw.GetTimelineProperties()?.Position()?.Duration;
            if verify_seek(start, requested, target, actual.position_ticks, published)
                != Err(SeekProofError::PlayerPosition)
            {
                return Err(
                    "negative echo control did not fail actual native movement proof".into(),
                );
            }
            println!(
                "NEGATIVE_ECHO_REJECTED player={} published={published}",
                actual.position_ticks
            );
        } else {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                let published = raw.GetTimelineProperties()?.Position()?.Duration;
                println!("PUBLISHED_AFTER_SEEK start={start} position={published} player_observed={} player_at_us={}",
                    actual.position_ticks, actual.at_us);
                match verify_seek(start, requested, target, actual.position_ticks, published) {
                    Ok(()) => break,
                    Err(SeekProofError::PublishedPosition) if Instant::now() < until => {}
                    Err(error) => return Err(format!("native seek proof failed: {error:?}").into()),
                }
                thread::sleep(Duration::from_millis(20));
            }
            let (before_pause, _) = find_owned(backend, manager, app, title, "before-pause")?;
            if before_pause.session != info.session {
                return Err("owned native token changed before pause".into());
            }
            require_backend_ok(await_backend_command(
                backend,
                MediaCommandRequest {
                    request_id: 32,
                    session: before_pause.session,
                    expected_revision: before_pause.revision,
                    command: MediaCommand::Pause,
                },
            ))?;
            if sync_line(&lines, Duration::from_secs(3))? != "PLAYBACK_ACK pause" {
                return Err("owned pause callback missing".into());
            }
            observe_state(
                &mut helper,
                &lines,
                &mut sequence,
                MediaPlaybackState::Paused,
            )?;
            let (paused, _) = find_owned(backend, manager, app, title, "after-pause")?;
            println!(
                "PAUSE_IDENTITY before={:?} after={:?} same_token={}",
                info.session,
                paused.session,
                info.session == paused.session
            );
            if paused.session != info.session {
                return Err(
                    "selected native view retired while paused; no stale Play command sent".into(),
                );
            }
            if paused.state != audio_backend::PlaybackState::Paused
                || backend.current_media_session()? != Some(paused.session)
            {
                return Err(
                    "same live paused token is not the production-selected resumable session"
                        .into(),
                );
            }
            require_backend_ok(await_backend_command(
                backend,
                MediaCommandRequest {
                    request_id: 33,
                    session: paused.session,
                    expected_revision: paused.revision,
                    command: MediaCommand::Play,
                },
            ))?;
            if sync_line(&lines, Duration::from_secs(3))? != "PLAYBACK_ACK play" {
                return Err("owned Play callback missing".into());
            }
            observe_state(
                &mut helper,
                &lines,
                &mut sequence,
                MediaPlaybackState::Playing,
            )?;
            let (playing, _) = find_owned(backend, manager, app, title, "after-play")?;
            if playing.session != info.session
                || playing.state != audio_backend::PlaybackState::Playing
            {
                return Err("backend did not preserve the same native player on resume".into());
            }
        }
        let pid = helper.0.id();
        input(&mut helper, "quit")?;
        let status = helper.0.wait()?;
        println!("OWN_CHILD_EXIT pid={pid} success={}", status.success());
        if !status.success() {
            return Err("owned helper failed during shutdown".into());
        }
        wait_backend_gone(backend, app, title)?;
        println!("OWN_CATALOG_GONE pid={pid}");
        if check_removal {
            let old = MediaCommandRequest {
                request_id: 34,
                session: info.session,
                expected_revision: info.revision,
                command: MediaCommand::SeekTo {
                    position_ms: SEEK_TARGET_MS as u64,
                },
            };
            match await_backend_command(backend, old) {
                Ok(Err(BackendError::DeviceGone | BackendError::StaleRevision)) => {}
                Err(BackendError::DeviceGone | BackendError::StaleRevision) => {}
                _ => return Err("removed backend session still accepted a command".into()),
            }
        }
        Ok(())
    }

    fn owned_seek_test() -> Result<()> {
        let _apartment = Apartment::open()?;
        let manager = NativeManager::RequestAsync()?.join()?;
        let mut backend = WindowsBackend::new()?;
        for (echo, check_removal) in [(false, false), (true, true)] {
            let nonce = nonce()?;
            let app = format!("BifrostSeekProbe.{nonce}");
            let title = format!("Bifrost seek probe {nonce}");
            if backend
                .media_sessions()?
                .iter()
                .any(|info| info.app() == app && info.title() == title)
            {
                return Err("static guard: new owned seek identity already exists".into());
            }
            let result = owned_seek_pair(&mut backend, &manager, &app, &title, echo, check_removal);
            if let Err(error) = result {
                let gone = wait_backend_gone(&mut backend, &app, &title);
                println!("OWN_ERROR_CLEANUP catalog_gone={}", gone.is_ok());
                return match gone {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(format!("{error}; owned catalog cleanup unverified: {cleanup}").into())
                    }
                };
            }
        }
        println!("PASS own backend declared published-offset + independent native getter + pause/play + echo + removal; not OS-wide origin or remote-worker E2E");
        Ok(())
    }

    #[cfg(test)]
    mod seek_tests {
        use super::*;

        fn candidate(token: u64) -> MediaInfo {
            MediaInfo::new(
                "own-fixture".into(),
                "own fixture".into(),
                String::new(),
                audio_backend::MediaSessionToken(token),
                1,
                audio_backend::PlaybackState::Paused,
                audio_backend::MediaCommands::default(),
            )
            .unwrap()
        }

        #[test]
        fn exact_owned_current_disambiguates_multiple_native_views() {
            let candidates = [candidate(11), candidate(22)];
            assert_eq!(
                admit_owned(&candidates, Some(audio_backend::MediaSessionToken(22))),
                Some(OwnAdmission::Current(audio_backend::MediaSessionToken(22)))
            );
        }

        #[test]
        fn absent_or_foreign_current_cannot_pick_first_owned_view() {
            let candidates = [candidate(11), candidate(22)];
            for current in [None, Some(audio_backend::MediaSessionToken(99))] {
                assert_eq!(admit_owned(&candidates, current), None);
            }
            assert_eq!(admit_owned(&[], None), None);
        }

        #[test]
        fn unique_noncurrent_candidate_is_primitive_only_and_unknown_identity_is_rejected() {
            let mut candidates = [candidate(11)];
            assert_eq!(
                admit_owned(&candidates, None),
                Some(OwnAdmission::PrimitiveOnly(
                    audio_backend::MediaSessionToken(11)
                ))
            );
            candidates[0].session = audio_backend::MediaSessionToken(0);
            assert_eq!(admit_owned(&candidates, None), None);
            candidates[0].session = audio_backend::MediaSessionToken(11);
            candidates[0].revision = u64::MAX;
            assert_eq!(admit_owned(&candidates, None), None);
        }

        #[test]
        fn native_evidence_keeps_request_player_and_published_domains_distinct() {
            let start = 600_000_000;
            let target = 200_000_000;
            assert_eq!(
                verify_seek(start, start + target, target, target, start + target),
                Ok(())
            );
            assert_eq!(
                verify_seek(start, start + target, target, 0, start + target),
                Err(SeekProofError::PlayerPosition)
            );
            assert_eq!(
                verify_seek(start, start + target, target, target, start),
                Err(SeekProofError::PublishedPosition)
            );
            assert_eq!(
                verify_seek(start, target, target, target, start + target),
                Err(SeekProofError::RequestCoordinate)
            );
            assert_eq!(
                verify_seek(i64::MAX, 0, 1, 1, 0),
                Err(SeekProofError::Overflow)
            );
        }

        #[test]
        fn native_provider_rejects_bad_range_without_clamp_or_saturation() {
            assert_eq!(
                provider_seek_offset(800_000_000, 600_000_000, 600_000_000),
                Some(200_000_000)
            );
            assert_eq!(
                provider_seek_offset(599_999_999, 600_000_000, 600_000_000),
                None
            );
            assert_eq!(
                provider_seek_offset(1_200_000_001, 600_000_000, 600_000_000),
                None
            );
            assert_eq!(provider_seek_offset(i64::MAX, i64::MIN, 600_000_000), None);
            assert_eq!(provider_seek_offset(600_000_000, 600_000_000, 0), None);
        }
    }

    async fn worker_child() -> Result<()> {
        let mut endpoint = String::new();
        let mut key = String::new();
        std::io::stdin().read_line(&mut endpoint)?;
        std::io::stdin().read_line(&mut key)?;
        let key = key.trim();
        if key.len() != 64 {
            return Err("invalid private worker token".into());
        }
        let mut token = [0; 32];
        for (i, byte) in token.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&key[i * 2..i * 2 + 2], 16)?;
        }
        let io = transport::connect(&Endpoint::pipe(endpoint.trim().to_owned())).await?;
        tokio::time::timeout(
            LIMIT,
            bifrost_audio::run_connection(io, LaunchToken::new(token.into())),
        )
        .await??;
        Ok(())
    }
    struct Worker {
        link: AudioLink<transport::ClientStream>,
        child: OwnedChild,
    }
    async fn worker(backend: i32, label: &str) -> Result<Worker> {
        let endpoint_name = format!(r"\\.\pipe\bifrost-owned-media-{}-{label}", nonce()?);
        let endpoint = Endpoint::pipe(endpoint_name.clone());
        let mut listener = transport::listen(&endpoint)?;
        let token = random::<32>()?;
        let mut child = child("--worker")?;
        input(&mut child, &endpoint_name)?;
        input(
            &mut child,
            &token.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        )?;
        let io = listener.accept(&SameUser::current()).await?;
        let mut link = AudioLink::core(io, 1)?;
        link.send(
            1,
            Msg::Attach(w::Attach {
                launch_token: token.to_vec(),
                backend,
                core_silence_ms: 1000,
            }),
        )
        .await?;
        match link.recv().await?.frame.msg {
            Some(Msg::Ready(ready)) if ready.backend == if backend == 1 { 4 } else { 2 } => {}
            _ => return Err("worker attach failed".into()),
        }
        Ok(Worker { link, child })
    }
    struct Harness {
        source: Worker,
        discard: Worker,
        heartbeat: tokio::time::Interval,
        active: bool,
        serial: u64,
    }
    impl Harness {
        async fn next(&mut self) -> Result<(bool, u64, Msg)> {
            loop {
                tokio::select! {
                    frame = self.source.link.recv() => {
                        let frame = frame?.frame;
                        return Ok((true, frame.request_id, frame.msg.ok_or("empty source frame")?));
                    }
                    frame = self.discard.link.recv() => {
                        let frame = frame?.frame;
                        return Ok((false, frame.request_id, frame.msg.ok_or("empty listener frame")?));
                    }
                    _ = self.heartbeat.tick() => {
                        self.serial += 1;
                        for worker in [&mut self.source, &mut self.discard] {
                            worker.link.send(0, Msg::Ping(w::Ping { serial: self.serial })).await?;
                            if self.active {
                                worker.link.send(1000 + self.serial, Msg::RenewSession(w::RenewSession { session_id: SESSION.to_vec(), ttl_ms: 1000 })).await?;
                            }
                        }
                    }
                }
            }
        }
    }
    fn check(msg: &Msg) -> Result<()> {
        if let Msg::Error(error) = msg {
            return Err(format!(
                "native worker unavailable/error {} (no fallback)",
                error.code
            )
            .into());
        }
        if let Msg::SessionEvent(event) = msg {
            if event.reason != 0 {
                return Err(format!("session stopped: {}", event.reason).into());
            }
        }
        Ok(())
    }
    fn owned<'a>(
        snapshot: &'a w::MediaSessions,
        app: &str,
        title: &str,
    ) -> Result<Option<&'a w::MediaInfo>> {
        let matches: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|s| {
                Some(s.token) == snapshot.current_token && s.app_id == app && s.title == title
            })
            .collect();
        if matches.len() > 1 {
            return Err("ambiguous owned helper; no command sent".into());
        }
        Ok(matches.first().copied())
    }
    fn start(role: i32, port: u16, peer: u16, forward: &[u8; 32], reverse: &[u8; 32]) -> Msg {
        Msg::StartSession(w::StartSession {
            session_id: SESSION.to_vec(),
            role,
            profile: 1,
            capture: (role == 1).then_some(w::CaptureSpec {
                kind: 1,
                device: None,
            }),
            render: (role == 2).then_some(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(1)),
            }),
            local_bind_ip: "127.0.0.1".into(),
            local_udp_port: u32::from(port),
            peer_ip: "127.0.0.1".into(),
            peer_udp_port: u32::from(peer),
            forward: Some(w::DirectionMaterial {
                key: forward.to_vec(),
                nonce_prefix: vec![51; 4],
            }),
            reverse: Some(w::DirectionMaterial {
                key: reverse.to_vec(),
                nonce_prefix: vec![52; 4],
            }),
            local_scope_id: 0,
            peer_scope_id: 0,
            initial_authorization_ms: 1000,
            source_local_sound_released: false,
        })
    }

    async fn acceptance() -> Result<()> {
        let identity = nonce()?;
        let app = format!("BifrostTest.{identity}");
        let title = format!("Bifrost test tone {identity}");
        let mut helper = child("--helper")?;
        let mut acknowledgements = lines(&mut helper)?;
        input(&mut helper, &identity)?;
        if acknowledgements.recv().await.as_deref() != Some("READY") {
            return Err("owned unpackaged SMTC helper unavailable; no fallback".into());
        }
        let mut harness = Harness {
            source: worker(1, "source").await?,
            discard: worker(2, "discard").await?,
            heartbeat: tokio::time::interval(Duration::from_millis(100)),
            active: false,
            serial: 0,
        };
        // Explicitly disabled, with a Ping barrier: no discovery before consent.
        harness
            .source
            .link
            .send(2, Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }))
            .await?;
        harness
            .source
            .link
            .send(0, Msg::Ping(w::Ping { serial: 9000 }))
            .await?;
        loop {
            let (source, _, msg) = harness.next().await?;
            check(&msg)?;
            if source {
                match msg {
                    Msg::MediaSessions(snapshot)
                        if !snapshot.sessions.is_empty() || snapshot.current_token.is_some() =>
                    {
                        return Err("metadata leaked before consent".into())
                    }
                    Msg::Pong(pong) if pong.serial == 9000 => break,
                    _ => {}
                }
            }
        }
        println!("PASS no metadata before consent");
        let a = UdpSocket::bind("127.0.0.1:0")?;
        let b = UdpSocket::bind("127.0.0.1:0")?;
        let (a_port, b_port) = (a.local_addr()?.port(), b.local_addr()?.port());
        if [a_port, b_port].contains(&47902) {
            return Err("reserved port selected; no session started".into());
        }
        let forward = random::<32>()?;
        let reverse = random::<32>()?;
        drop((a, b));
        harness
            .discard
            .link
            .send(3, start(2, b_port, a_port, &forward, &reverse))
            .await?;
        harness
            .source
            .link
            .send(3, start(1, a_port, b_port, &forward, &reverse))
            .await?;
        harness.active = true;
        loop {
            let (source, _, msg) = harness.next().await?;
            check(&msg)?;
            if source {
                if let Msg::MediaSessions(snapshot) = &msg {
                    if !snapshot.sessions.is_empty() || snapshot.current_token.is_some() {
                        return Err(
                            "metadata leaked during authorized Source before watch consent".into(),
                        );
                    }
                }
                if let Msg::Health(health) = msg {
                    if health
                        .sessions
                        .iter()
                        .any(|s| s.session_id == SESSION && s.sending == Some(true))
                    {
                        break;
                    }
                }
            }
        }
        harness
            .source
            .link
            .send(4, Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
            .await?;
        let initial = loop {
            let (source, _, msg) = harness.next().await?;
            check(&msg)?;
            if source {
                if let Msg::MediaSessions(snapshot) = msg {
                    if let Some(info) = owned(&snapshot, &app, &title)? {
                        if snapshot.current_token == Some(info.token) {
                            break snapshot;
                        }
                    }
                }
            }
        };
        // Other-session metadata is kept only in memory, never printed or commanded.
        let other_tokens: Vec<_> = initial
            .sessions
            .iter()
            .filter(|s| s.app_id != app || s.title != title)
            .map(|s| s.token)
            .collect();
        println!(
            "PASS exact owned identity; {} other sessions observed read-only",
            other_tokens.len()
        );
        for (id, command, event, capability) in [
            (10, 2, "ACK pause", 2),
            (11, 1, "ACK play", 1),
            (12, 4, "ACK next", 8),
        ] {
            // Refresh immediately before each command; never substitute OS current.
            harness
                .source
                .link
                .send(
                    id + 100,
                    Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }),
                )
                .await?;
            let latest = loop {
                let (source, _, msg) = harness.next().await?;
                check(&msg)?;
                if source {
                    if let Msg::MediaSessions(snapshot) = msg {
                        break snapshot;
                    }
                }
            };
            let info = owned(&latest, &app, &title)?.ok_or("owned helper disappeared")?;
            if latest.current_token != Some(info.token)
                || other_tokens.contains(&info.token)
                || info.revision == 0
            {
                return Err("owned helper is not uniquely current; no command sent".into());
            }
            if info.commands & capability == 0 {
                return Err("owned helper command unsupported; no fallback".into());
            }
            // This is the only media-command send site. Both token and revision come
            // exclusively from the nonce+AppUserModelID match above, never first/current.
            harness
                .source
                .link
                .send(
                    id,
                    Msg::MediaCommand(w::MediaCommand {
                        token: info.token,
                        expected_revision: info.revision,
                        command,
                        position_ms: None,
                    }),
                )
                .await?;
            let (mut acknowledged, mut completed) = (false, false);
            while !acknowledged || !completed {
                tokio::select! {
                    line = acknowledgements.recv(), if !acknowledged => {
                        if line.as_deref() != Some(event) { return Err("unexpected owned helper acknowledgement".into()); }
                        acknowledged = true;
                    }
                    frame = harness.next() => {
                        let (source, request, msg) = frame?;
                        check(&msg)?;
                        if source {
                            match msg {
                                Msg::MediaCommandResult(result) if request == id => {
                                    if result.result != 1 { return Err(format!("owned command refused/status {} (no retry/fallback)", result.result).into()); }
                                    completed = true;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            println!(
                "PASS owned {} command and helper acknowledgement",
                &event[4..]
            );
        }
        harness
            .source
            .link
            .send(20, Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }))
            .await?;
        loop {
            let (source, _, msg) = harness.next().await?;
            check(&msg)?;
            if source {
                if let Msg::MediaSessions(snapshot) = msg {
                    if snapshot.sessions.is_empty() && snapshot.current_token.is_none() {
                        break;
                    }
                    return Err("metadata remained after consent revocation".into());
                }
            }
        }
        for worker in [&mut harness.source, &mut harness.discard] {
            worker.link.send(21, Msg::Shutdown(w::Shutdown {})).await?;
        }
        input(&mut helper, "quit")?;
        // RAII kills/reaps only these owned processes, including on timeout/error.
        let _ = (&harness.source.child, &harness.discard.child);
        println!("PASS consent revocation; only exact helper token was commanded");
        Ok(())
    }
    pub fn main() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() == 1 && args[0] == "--owned-seek-test" {
            return owned_seek_test();
        }
        if args.first().map(String::as_str) == Some("--owned-test")
            && args.len() == 2
            && args.get(1).map(String::as_str) == Some("--seek-helper")
        {
            return seek_helper(false);
        }
        if args.first().map(String::as_str) == Some("--owned-test")
            && args.len() == 2
            && args.get(1).map(String::as_str) == Some("--seek-echo-helper")
        {
            return seek_helper(true);
        }
        if args.first().map(String::as_str) != Some("--owned-test") || args.len() > 2 {
            return Err("explicit consent required: --owned-test or --owned-seek-test (owned silent seek probe; no other sessions commanded)".into());
        }
        if args.get(1).map(String::as_str) == Some("--helper") {
            return helper();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        match args.get(1).map(String::as_str) {
            Some("--worker") => runtime.block_on(worker_child()),
            None => runtime.block_on(async { tokio::time::timeout(Duration::from_secs(20), acceptance()).await.map_err(|_| "owned media acceptance timed out: SMTC identity/current selection or native audio unavailable; no fallback")? }),
            _ => Err("unknown owned-test child mode".into()),
        }
    }
}
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::main()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Unsupported: windows_media_e2e requires native Windows SMTC; no fallback");
    std::process::exit(1);
}
