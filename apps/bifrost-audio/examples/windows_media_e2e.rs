//! Opt-in native SMTC acceptance; never sends commands to an existing player.
//! cargo run -p bifrost-audio --example windows_media_e2e -- --owned-test
//! Plays only a generated helper tone. Native process-excluding capture is sent
//! over encrypted loopback to an explicitly dry, memory-only discard listener.
//! No microphone, endpoint settings, installed Bifrost, or input synthesis is used.
#[cfg(windows)]
mod native {
    use ipc::{
        audio_worker::{AudioLink, LaunchToken},
        peer::SameUser,
        transport::{self, Endpoint},
    };
    use protocol::audio_worker::{self as w, audio_frame::Msg};
    use ring::rand::{SecureRandom, SystemRandom};
    use std::{
        error::Error,
        io::{BufRead, BufReader, Write},
        net::UdpSocket,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::Duration,
    };
    use windows::{
        core::{HSTRING, PCWSTR},
        Foundation::{TimeSpan, TypedEventHandler},
        Media::{
            Core::MediaSource, MediaPlaybackStatus, MediaPlaybackType, Playback::MediaPlayer,
            SystemMediaTransportControls, SystemMediaTransportControlsButton,
            SystemMediaTransportControlsButtonPressedEventArgs,
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
        if args.first().map(String::as_str) != Some("--owned-test") || args.len() > 2 {
            return Err("explicit consent required: --owned-test (generated audible helper tone; native loopback capture discarded in memory)".into());
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
