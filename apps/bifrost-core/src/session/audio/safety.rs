//! A constant-space revocation latch independent of the bounded GUI work queue.
use super::*;

#[derive(Clone, Copy, Default)]
pub(super) struct Latch {
    pub(super) generation: u64,
    mute_generation: u64,
}

/// Only an actually owned local guard is published; the handle cannot retarget a restart.
#[derive(Clone)]
pub(super) struct Target {
    worker_generation: u64,
    endpoint_generation: u64,
    handle: AudioHandle,
    baseline: u64,
}

pub(super) fn publish_target(
    targets: &watch::Sender<Option<Target>>,
    latch: &watch::Receiver<Latch>,
    owned: Option<(u64, u64, &AudioHandle)>,
) {
    // Serialize target publication against admission. A newer safety action must
    // either precede this baseline or remain pending after the target is visible.
    let baseline = latch.borrow();
    targets.send_if_modified(|current| {
        let identity = owned
            .as_ref()
            .map(|(worker, endpoint, _)| (*worker, *endpoint));
        if current
            .as_ref()
            .map(|target| (target.worker_generation, target.endpoint_generation))
            == identity
        {
            return false;
        }
        *current = owned.map(|(worker_generation, endpoint_generation, handle)| Target {
            worker_generation,
            endpoint_generation,
            handle: handle.for_generation(worker_generation),
            baseline: baseline.generation,
        });
        true
    });
}

pub(super) async fn watch_safety(
    mut latch: watch::Receiver<Latch>,
    mut targets: watch::Receiver<Option<Target>>,
    mut stopping: watch::Receiver<bool>,
) {
    let mut identity = None;
    let mut applied = 0;
    loop {
        if *stopping.borrow() {
            return;
        }
        let target = targets.borrow_and_update().clone();
        let current = *latch.borrow_and_update();
        if let Some(target) = target {
            // Include the publication baseline: clear/reacquire can coalesce back to
            // the same physical endpoint without changing its native generation.
            let key = (
                target.worker_generation,
                target.endpoint_generation,
                target.baseline,
            );
            if identity != Some(key) {
                identity = Some(key);
                applied = target.baseline;
            }
            if let Some(mute) = current.pending(applied) {
                applied = current.generation;
                let release = target.handle.ptt_release_all();
                let result = if mute {
                    let (release, mute) = tokio::join!(
                        release,
                        target.handle.set_mic_mute(target.endpoint_generation, true)
                    );
                    release.and(mute)
                } else {
                    release.await
                };
                if let Err(error) = result {
                    tracing::warn!(?error, "independent microphone safety command failed");
                }
            }
        } else {
            identity = None;
        }
        tokio::select! {
            changed = latch.changed() => if changed.is_err() { return; },
            changed = targets.changed() => if changed.is_err() { return; },
            changed = stopping.changed() => if changed.is_err() { return; },
        }
    }
}

pub(super) struct Command {
    pub(super) request: Request,
    generation: u64,
}

impl Command {
    pub(super) fn superseded(&self, current: Latch) -> bool {
        activation(&self.request.message)
            && (self.generation != current.generation || current.generation == u64::MAX)
    }
}

impl Latch {
    pub(super) fn pending(self, applied: u64) -> Option<bool> {
        (self.generation != applied).then_some(self.mute_generation > applied)
    }
}

fn activation(message: &gui::gui_frame::Msg) -> bool {
    matches!(message,
        gui::gui_frame::Msg::SetMicrophoneMute(v) if !v.muted
    ) || matches!(message, gui::gui_frame::Msg::SetPtt(v) if v.pressed)
        // Full preferences can reacquire or restore the microphone guard as well.
        || matches!(message, gui::gui_frame::Msg::SetAudio(_))
}

pub(super) fn submit(
    queue: &mpsc::Sender<Command>,
    safety: &watch::Sender<Latch>,
    request: Request,
) -> Result<(), (u64, mpsc::Sender<Outgoing>)> {
    let mute = matches!(&request.message, gui::gui_frame::Msg::SetMicrophoneMute(v) if v.muted);
    let release = matches!(&request.message, gui::gui_frame::Msg::SetPtt(v) if !v.pressed);
    if mute || release {
        // Global microphone authority: a safety action supersedes every older activation,
        // including another connection's queued activation, before admission can fail.
        safety.send_modify(|latch| {
            latch.generation = latch.generation.saturating_add(1);
            if mute {
                latch.mute_generation = latch.generation;
            }
        });
    }
    let generation = safety.borrow().generation;
    queue
        .try_send(Command {
            request,
            generation,
        })
        .map_err(|error| {
            let request = error.into_inner().request;
            (request.request_id, request.out)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, message: gui::gui_frame::Msg) -> Request {
        let (out, _) = mpsc::channel(1);
        Request {
            request_id: id,
            connection_id: [1; 16],
            message,
            out,
        }
    }

    #[test]
    fn saturated_queue_latches_safety_and_polling_cannot_rearm_old_activation() {
        for mute in [false, true] {
            let (queue, mut incoming) = mpsc::channel(1);
            let (safety, mut latch) = watch::channel(Latch::default());
            let press = gui::gui_frame::Msg::SetPtt(gui::SetPtt {
                pressed: true,
                expected_revision: 1,
                ..Default::default()
            });
            assert!(submit(&queue, &safety, request(1, press)).is_ok());
            let action = if mute {
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                })
            } else {
                gui::gui_frame::Msg::SetPtt(gui::SetPtt {
                    pressed: false,
                    expected_revision: 0,
                    ..Default::default()
                })
            };
            let (id, _) = submit(&queue, &safety, request(2, action)).unwrap_err();
            assert_eq!(id, 2);
            let revoked = *latch.borrow_and_update();
            assert_eq!(revoked.pending(0), Some(mute));
            let old = incoming.try_recv().unwrap();
            assert!(old.superseded(revoked));
            // Continued ordinary work never acknowledges or clears the revocation latch.
            for id in 3..100 {
                let poll = gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {});
                assert!(submit(&queue, &safety, request(id, poll)).is_ok());
                assert!(!incoming.try_recv().unwrap().superseded(*latch.borrow()));
                assert!(old.superseded(*latch.borrow()));
                assert_eq!(latch.borrow().pending(0), Some(mute));
            }
        }
    }

    #[test]
    fn queued_preferences_cannot_restore_the_guard_after_a_newer_safety_mute() {
        let (queue, mut incoming) = mpsc::channel(1);
        let (safety, latch) = watch::channel(Latch::default());
        let preferences = wire::encode_config(
            &config::AudioConfig::default(),
            model::AudioProfile::Ethernet,
        )
        .unwrap();
        assert!(submit(
            &queue,
            &safety,
            request(
                1,
                gui::gui_frame::Msg::SetAudio(gui::SetAudio {
                    config: Some(preferences),
                    expected_revision: 1,
                })
            )
        )
        .is_ok());
        assert!(submit(
            &queue,
            &safety,
            request(
                2,
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                })
            )
        )
        .is_err());
        assert!(incoming.try_recv().unwrap().superseded(*latch.borrow()));
    }

    #[test]
    fn coalesced_release_never_erases_a_pending_mute() {
        let (queue, _incoming) = mpsc::channel(1);
        let (safety, latch) = watch::channel(Latch::default());
        let _ = submit(
            &queue,
            &safety,
            request(
                1,
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                }),
            ),
        );
        let _ = submit(
            &queue,
            &safety,
            request(
                2,
                gui::gui_frame::Msg::SetPtt(gui::SetPtt {
                    pressed: false,
                    expected_revision: 0,
                    ..Default::default()
                }),
            ),
        );
        assert_eq!(latch.borrow().pending(0), Some(true));
        assert_eq!(latch.borrow().pending(1), Some(false));
    }

    struct DryRoot(std::path::PathBuf);
    impl Drop for DryRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn dry_child() -> (
        DryRoot,
        AudioHandle,
        mpsc::Receiver<AudioSupervisorEvent>,
        u64,
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = DryRoot(std::env::temp_dir().join(format!(
            "bf-safe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )));
        let program = std::env::var_os("BIFROST_BIN")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join(format!("bifrost{}", std::env::consts::EXE_SUFFIX))
            });
        assert!(
            program.is_file(),
            "build the real bifrost binary or set BIFROST_BIN"
        );
        let (handle, mut events) = AudioSupervisor::start(AudioSupervisorConfig {
            program,
            private_dir: root.0.join("child"),
            logs_dir: root.0.join("logs"),
            backend: worker::BackendKind::Dry,
        })
        .await
        .unwrap();
        let generation = loop {
            if let AudioSupervisorEvent::Ready {
                generation, ready, ..
            } = next(&mut events).await
            {
                assert_eq!(ready.backend, worker::BackendKind::Dry as i32);
                break generation;
            }
        };
        handle.probe().await.unwrap();
        (root, handle, events, generation)
    }

    async fn next(events: &mut mpsc::Receiver<AudioSupervisorEvent>) -> AudioSupervisorEvent {
        match timeout(Duration::from_secs(8), events.recv())
            .await
            .expect("Dry safety event deadline")
            .expect("Dry safety event channel")
        {
            AudioSupervisorEvent::Failed { reason, .. } => panic!("Dry child failed: {reason}"),
            AudioSupervisorEvent::CommandRejected { error, .. } => {
                panic!("Dry command rejected: {error:?}")
            }
            event => event,
        }
    }

    async fn mute_snapshot(
        events: &mut mpsc::Receiver<AudioSupervisorEvent>,
        expected: u64,
    ) -> worker::MuteSnapshot {
        loop {
            if let AudioSupervisorEvent::MuteSnapshot {
                request_id,
                snapshot,
                ..
            } = next(events).await
            {
                if request_id == expected {
                    return snapshot;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_dry_watcher_cuts_talk_while_production_config_write_is_locked() {
        for mute in [false, true] {
            let (root, handle, mut events, worker_generation) = dry_child().await;
            let acquire = handle
                .acquire_mic_mute(worker::DeviceSelection {
                    selector: Some(worker::device_selection::Selector::FollowDefault(2)),
                })
                .await
                .unwrap();
            let owned = mute_snapshot(&mut events, acquire).await;
            assert!(owned.guard_active);
            let id = handle.set_mic_mute(owned.generation, true).await.unwrap();
            assert!(mute_snapshot(&mut events, id).await.muted);
            let press = handle.ptt_grant(73, 250).await.unwrap();
            let (mut pressed, mut opened) = (false, false);
            while !pressed || !opened {
                match next(&mut events).await {
                    AudioSupervisorEvent::PttEvent {
                        request_id, event, ..
                    } if request_id == press => {
                        assert_eq!(event.kind, worker::PttEventKind::Pressed as i32);
                        pressed = true;
                    }
                    AudioSupervisorEvent::MuteSnapshot { snapshot, .. } if !snapshot.muted => {
                        assert!(snapshot.guard_active);
                        opened = true;
                    }
                    _ => {}
                }
            }
            let (admission, actor_latch) = watch::channel(Latch::default());
            let (targets, target_rx) = watch::channel(None);
            let (stopping, stop_rx) = watch::channel(false);
            publish_target(
                &targets,
                &actor_latch,
                Some((worker_generation, owned.generation, &handle)),
            );
            let safety_task = tokio::spawn(watch_safety(actor_latch.clone(), target_rx, stop_rx));
            let renew_handle = handle.clone();
            let renewing = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                loop {
                    tick.tick().await;
                    renew_handle.ptt_grant(73, 250).await.unwrap();
                }
            });
            let path = root.0.join("config.toml");
            let mut config = config::Config::default();
            config.network.listen_address = "127.0.0.1".to_owned();
            config.save(&path).unwrap();
            let config_lock = config::lock::FileLock::acquire(&path).unwrap();
            let (entered, started_write) = tokio::sync::oneshot::channel();
            let blocked_write = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                config::save_audio_locked(&path, config.audio)
            });
            started_write.await.unwrap();
            assert!(!blocked_write.is_finished());
            // Leave the actor latch unconsumed while its actual persistence path
            // waits on a real OS config lock; only the independent watcher progresses.
            let (queue, _blocked_actor_queue) = mpsc::channel(1);
            assert!(submit(
                &queue,
                &admission,
                request(
                    1,
                    gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {})
                )
            )
            .is_ok());
            let action = if mute {
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                })
            } else {
                gui::gui_frame::Msg::SetPtt(gui::SetPtt {
                    pressed: false,
                    expected_revision: 0,
                    ..Default::default()
                })
            };
            assert!(submit(&queue, &admission, request(2, action)).is_err());
            let (mut revoked, mut remuted) = (false, false);
            timeout(Duration::from_millis(350), async {
                while !revoked || !remuted {
                    assert!(
                        !blocked_write.is_finished(),
                        "config write must remain blocked"
                    );
                    match next(&mut events).await {
                        AudioSupervisorEvent::PttEvent { event, .. }
                            if event.grant_id == 0
                                && event.kind == worker::PttEventKind::Released as i32
                                && event.reason == worker::PttReleaseReason::Revoked as i32 =>
                        {
                            revoked = true
                        }
                        AudioSupervisorEvent::MuteSnapshot { snapshot, .. } if snapshot.muted => {
                            assert!(snapshot.guard_active);
                            remuted = true;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("independent safety must cut within the 250 ms lease + 100 ms allowance");
            assert!(!blocked_write.is_finished());
            assert!(
                actor_latch.has_changed().unwrap(),
                "actor has not consumed safety"
            );
            let until = tokio::time::sleep(Duration::from_millis(350));
            tokio::pin!(until);
            let mut refused_old_renewal = false;
            loop {
                tokio::select! {
                    _ = &mut until => break,
                    event = next(&mut events) => match event {
                        AudioSupervisorEvent::MuteSnapshot { snapshot, .. } => {
                            assert!(snapshot.muted, "old renewal reopened the owned endpoint");
                        }
                        AudioSupervisorEvent::PttEvent { event, .. } if event.grant_id == 73 => {
                            assert_eq!(event.kind, worker::PttEventKind::Refused as i32);
                            refused_old_renewal = true;
                        }
                        _ => {}
                    }
                }
            }
            assert!(refused_old_renewal);
            assert!(!blocked_write.is_finished());
            renewing.abort();
            let _ = renewing.await;
            // Ordinary observations of the same endpoint must not acknowledge a latch.
            publish_target(
                &targets,
                &actor_latch,
                Some((worker_generation, owned.generation, &handle)),
            );
            assert_eq!(targets.borrow().as_ref().unwrap().baseline, 0);
            // A safety mute must also close a persistent unmute with no remaining hold.
            let id = handle.set_mic_mute(owned.generation, false).await.unwrap();
            assert!(!mute_snapshot(&mut events, id).await.muted);
            assert!(submit(
                &queue,
                &admission,
                request(
                    3,
                    gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                        muted: true,
                        expected_revision: 0,
                    })
                )
            )
            .is_err());
            timeout(Duration::from_millis(350), async {
                loop {
                    if let AudioSupervisorEvent::MuteSnapshot { snapshot, .. } =
                        next(&mut events).await
                    {
                        if snapshot.muted {
                            assert!(snapshot.guard_active);
                            break;
                        }
                    }
                }
            })
            .await
            .expect("independent mute must not depend on an active hold");
            assert!(!blocked_write.is_finished());
            drop(config_lock);
            blocked_write.await.unwrap().unwrap();
            // Releasing/reacquiring creates a new actual guard. Older safety must not
            // be replayed onto it; its publication starts at the current latch baseline.
            targets.send_replace(None);
            let id = handle.release_mic_mute().await.unwrap();
            assert!(!mute_snapshot(&mut events, id).await.guard_active);
            let id = handle
                .acquire_mic_mute(worker::DeviceSelection {
                    selector: Some(worker::device_selection::Selector::FollowDefault(2)),
                })
                .await
                .unwrap();
            let replacement = mute_snapshot(&mut events, id).await;
            assert!(replacement.guard_active);
            publish_target(
                &targets,
                &actor_latch,
                Some((worker_generation, replacement.generation, &handle)),
            );
            assert_eq!(
                targets.borrow().as_ref().unwrap().baseline,
                admission.borrow().generation
            );
            no_native_controls(&mut events, Some(&replacement)).await;
            stopping.send_replace(true);
            safety_task.await.unwrap();
            handle.shutdown().await.unwrap();
        }
    }

    async fn no_native_controls(
        events: &mut mpsc::Receiver<AudioSupervisorEvent>,
        unchanged: Option<&worker::MuteSnapshot>,
    ) {
        let quiet = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                _ = &mut quiet => return,
                event = next(events) => {
                    // Acquisition also pushes an unsolicited ownership observation.
                    // Only an exact duplicate is harmless; new commands or state changes fail.
                    let duplicate = matches!(
                        &event,
                        AudioSupervisorEvent::MuteSnapshot { request_id: 0, snapshot, .. }
                            if unchanged == Some(snapshot)
                    );
                    assert!(
                        duplicate || !matches!(event,
                            AudioSupervisorEvent::MuteSnapshot { .. }
                            | AudioSupervisorEvent::PttEvent { .. }),
                        "absent/new target must not acquire or replay older safety: {event:?}",
                    );
                },
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_dry_watcher_without_scope_never_acquires_or_replays_older_safety() {
        let (_root, handle, mut events, worker_generation) = dry_child().await;
        let (admission, actor_latch) = watch::channel(Latch::default());
        let (targets, target_rx) = watch::channel(None);
        let (stopping, stop_rx) = watch::channel(false);
        let safety_task = tokio::spawn(watch_safety(actor_latch.clone(), target_rx, stop_rx));
        let (queue, _blocked_actor_queue) = mpsc::channel(1);
        assert!(submit(
            &queue,
            &admission,
            request(
                1,
                gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {})
            )
        )
        .is_ok());
        assert!(submit(
            &queue,
            &admission,
            request(
                2,
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                })
            )
        )
        .is_err());
        no_native_controls(&mut events, None).await;
        let id = handle
            .acquire_mic_mute(worker::DeviceSelection {
                selector: Some(worker::device_selection::Selector::FollowDefault(2)),
            })
            .await
            .unwrap();
        let owned = mute_snapshot(&mut events, id).await;
        assert!(owned.guard_active && !owned.muted);
        publish_target(
            &targets,
            &actor_latch,
            Some((worker_generation, owned.generation, &handle)),
        );
        assert_eq!(
            targets.borrow().as_ref().unwrap().baseline,
            admission.borrow().generation
        );
        no_native_controls(&mut events, Some(&owned)).await;
        // Only an action admitted after publication may affect this new ownership.
        assert!(submit(
            &queue,
            &admission,
            request(
                3,
                gui::gui_frame::Msg::SetMicrophoneMute(gui::SetMicrophoneMute {
                    muted: true,
                    expected_revision: 0,
                })
            )
        )
        .is_err());
        timeout(Duration::from_millis(350), async {
            loop {
                if let AudioSupervisorEvent::MuteSnapshot { snapshot, .. } = next(&mut events).await
                {
                    if snapshot.muted {
                        assert!(snapshot.guard_active);
                        break;
                    }
                }
            }
        })
        .await
        .expect("new safety action must survive publication ordering");
        stopping.send_replace(true);
        safety_task.await.unwrap();
        handle.shutdown().await.unwrap();
    }
}
