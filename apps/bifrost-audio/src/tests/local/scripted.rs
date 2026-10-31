use super::*;
use audio_backend::dry::DryMuteGuard;
use std::sync::{mpsc as sync_mpsc, LazyLock};

pub(super) enum Script {
    Event(ScriptEvent),
    CompleteMedia,
    Stall(Duration),
}
struct ScriptedBackend {
    dry: DryBackend,
    script: sync_mpsc::Receiver<Script>,
}
static FACTORIES: LazyLock<[Mutex<Option<ScriptedBackend>>; 2]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(None)));
fn factory<const INDEX: usize>() -> Result<ScriptedBackend, BackendError> {
    lock(&FACTORIES[INDEX]).take().ok_or(BackendError::Stopped)
}
impl Backend for ScriptedBackend {
    type Stream = DryStream;
    type Mute = DryMuteGuard;
    fn capabilities(&self) -> Capabilities {
        self.dry.capabilities()
    }
    fn probe_capabilities(&mut self) -> CapabilityProbe {
        self.dry.probe_capabilities()
    }
    fn devices(&self) -> Result<Vec<Device>, BackendError> {
        self.dry.devices()
    }
    fn defaults(&self) -> Result<Defaults, BackendError> {
        self.dry.defaults()
    }
    fn start_capture(
        &mut self,
        request: CaptureSpec,
        callback: CaptureCallback,
    ) -> Result<DryStream, BackendError> {
        self.dry.start_capture(request, callback)
    }
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<DryStream, BackendError> {
        self.dry.start_render(request, callback)
    }
    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError> {
        self.dry.capture_mute(selection)
    }
    fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<DryMuteGuard, BackendError> {
        self.dry.acquire_mute(selection)
    }
    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        self.dry.media_sessions()
    }
    fn current_media_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.dry.current_media_session()
    }
    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        self.dry.media_command(request)
    }
    fn cancel_media_commands(&mut self) {
        self.dry.cancel_media_commands();
    }
    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        self.dry.configure_ptt(hotkey, reserved)
    }
    fn poll_event(&mut self) -> Option<Event> {
        for _ in 0..64 {
            match self.script.try_recv() {
                Ok(Script::Event(event)) => self.dry.apply(event).expect("valid Dry script"),
                Ok(Script::CompleteMedia) => self.dry.pump_control(),
                Ok(Script::Stall(duration)) => thread::sleep(duration),
                Err(_) => break,
            }
        }
        self.dry.poll_event()
    }
}

pub(super) async fn child(index: usize) -> (Fixture, sync_mpsc::Sender<Script>) {
    let mut dry = DryBackend::new(Capabilities {
        system_capture: true,
        exclude_own_process_tree: true,
        endpoint_mute: true,
        media: true,
        global_ptt: true,
    });
    let format = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    let render = DeviceId::new("DRY_RENDER").unwrap();
    let capture = DeviceId::new("DRY_MIC").unwrap();
    for (id, direction) in [
        (render.clone(), Direction::Render),
        (capture.clone(), Direction::Capture),
    ] {
        dry.apply(ScriptEvent::UpsertDevice(Device {
            id,
            label: "Dry control endpoint".into(),
            direction,
            format,
        }))
        .unwrap();
    }
    let defaults = DefaultEndpoints {
        render: Some(render),
        capture: Some(capture),
    };
    dry.apply(ScriptEvent::SetDefaults(Defaults {
        multimedia: defaults.clone(),
        communications: defaults,
    }))
    .unwrap();
    while dry.poll_event().is_some() {}
    let (script, receiver) = sync_mpsc::channel();
    let slots = &*FACTORIES;
    assert!(lock(&slots[index])
        .replace(ScriptedBackend {
            dry,
            script: receiver
        })
        .is_none());
    let directory = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::socket(directory.path().join("audio.sock"));
    let mut listener = transport::listen(&endpoint).unwrap();
    let observed = Arc::new(Observed::default());
    let child_observed = observed.clone();
    let child_endpoint = endpoint.clone();
    let handle = tokio::spawn(async move {
        let io = transport::connect(&child_endpoint).await.unwrap();
        let mut link = AudioLink::worker(io);
        let mut attach = link.recv().await?;
        let token = attach.token.take().ok_or(RoleError::WorkerFailed)?;
        ipc::agent::verify_launch_token(&[71; 32], token.expose_secret())?;
        let request = attach.frame.request_id;
        if !matches!(
            attach.frame.msg,
            Some(Msg::Attach(w::Attach { backend: 2, .. }))
        ) {
            return Err(RoleError::WorkerFailed);
        }
        link.send(request, Msg::Ready(unavailable(2, false, 1)))
            .await?;
        let pump: worker::Pump<DryStream> = Box::new(move |stream| {
            stream.pump(480)?;
            child_observed.observe(stream);
            Ok(())
        });
        let factory = if index == 0 {
            factory::<0>
        } else {
            factory::<1>
        };
        serve(link, 2, factory, Some(pump), None).await
    });
    let io = listener.accept(&SameUser::current()).await.unwrap();
    let mut core = AudioLink::core(io, 9).unwrap();
    core.send(
        1,
        Msg::Attach(w::Attach {
            launch_token: vec![71; 32],
            backend: 2,
            core_silence_ms: contract::CORE_SILENCE_MS,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        next(&mut core).await,
        Msg::Ready(w::Ready { probed: false, .. })
    ));
    (
        Fixture {
            core,
            handle,
            observed,
            renew_ids: Vec::new(),
            _directory: directory,
        },
        script,
    )
}

pub(super) async fn next_frame(fixture: &mut Fixture) -> contract::Received {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut serial = 1;
        loop {
            tokio::select! {
                frame = fixture.core.recv() => return frame.unwrap(),
                _ = tick.tick() => {
                    fixture.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                    for id in fixture.renew_ids.clone() { renew(&mut fixture.core, id, 1000).await; }
                    serial += 1;
                }
            }
        }
    }).await.unwrap()
}
