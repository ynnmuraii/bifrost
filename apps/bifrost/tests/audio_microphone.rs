//! Real paired cores and GUI connections, with explicitly isolated Dry audio children.
//! Microphone facts come from the Listener's child, never a GUI command acknowledgement.

#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window};
use common::Unpaired;
use protocol::ipc::{self as wire, gui_frame};
use std::time::{Duration, Instant};

fn pair(label: &str) -> (Unpaired, Window, Window) {
    let two = Unpaired::start_audio(label);
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let ports = audio::ports(2);
    let mut config = audio::config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        &[source_id],
        false,
    );
    let microphone = config.microphone.as_mut().expect("microphone config");
    microphone.control_enabled = true;
    microphone.ptt_enabled = true;
    config.peers[0].allow_mic_control = true;
    listener.configure(config);
    let group = listener
        .wait("Listener mints its own group", |status| {
            status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id.len() == 16)
        })
        .config
        .expect("Listener config")
        .group_id;
    let mut config = audio::config(
        wire::AudioRole::Source,
        ports[1],
        &listener_id,
        std::slice::from_ref(&listener_id),
        false,
    );
    let microphone = config.microphone.as_mut().expect("microphone config");
    microphone.control_enabled = true;
    microphone.ptt_enabled = true;
    // An empty binding intentionally exercises the GUI button, not a desktop hook.
    assert!(microphone.ptt_hotkey.is_empty());
    source.configure(config);
    source.wait(
        "authenticated Hello resolves pending Source selection",
        |status| {
            status.config.as_ref().is_some_and(|config| {
                config.group_id == group && config.listener_node_id == listener_id
            }) && status
                .microphone
                .as_ref()
                .is_some_and(|mic| mic.control_active)
        },
    );
    listener.wait("actual Listener endpoint guard is acquired", |status| {
        status
            .microphone
            .as_ref()
            .is_some_and(|mic| mic.control_active)
    });
    assert_eq!(source.status().role, wire::AudioRole::Source as i32);
    assert!(listener.status().sessions.is_empty());
    assert!(source.status().sessions.is_empty());
    (two, listener, source)
}

fn microphone(status: &wire::AudioStatus, muted: bool, pressed: bool) -> bool {
    status.microphone.as_ref().is_some_and(|mic| {
        mic.control_active
            && mic.muted == muted
            && mic.ptt_pressed == pressed
            && !mic.capture_active
    })
}

fn agree(listener: &mut Window, source: &mut Window, muted: bool, pressed: bool) {
    let actual = listener.wait("Listener child reports microphone outcome", |status| {
        microphone(status, muted, pressed)
    });
    let remote = source.wait("Source sees Listener microphone outcome", |status| {
        microphone(status, muted, pressed)
    });
    let actual_mic = actual.microphone.expect("Listener microphone");
    let remote_mic = remote.microphone.expect("relayed Listener microphone");
    assert_eq!(actual_mic.muted, remote_mic.muted);
    assert_eq!(actual_mic.ptt_pressed, remote_mic.ptt_pressed);
    assert_eq!(actual_mic.control_active, remote_mic.control_active);
    assert!(actual.sessions.is_empty());
    assert!(remote.sessions.is_empty());
}

fn mute(window: &mut Window, muted: bool) -> u64 {
    let revision = window.status().revision;
    window.op(gui_frame::Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
        muted,
        expected_revision: revision,
    }));
    revision
}

fn ptt(source: &mut Window, pressed: bool) {
    let revision = source.status().revision;
    match source.request(gui_frame::Msg::SetPtt(wire::SetPtt {
        pressed,
        expected_revision: revision,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(result.ok, "PTT pressed={pressed}: {result:?}");
        }
        other => panic!("PTT pressed={pressed} answered {other:?}"),
    }
}

fn hold(listener: &mut Window, source: &mut Window, muted: bool, pressed: bool) {
    let until = Instant::now() + Duration::from_millis(650);
    while Instant::now() < until {
        let actual = listener.status();
        let remote = source.status();
        assert!(
            microphone(&actual, muted, pressed),
            "Listener during hold: {actual:?}"
        );
        assert!(
            microphone(&remote, muted, pressed),
            "Source during hold: {remote:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn source_gui_persistent_unmute_is_not_a_talk_lease_and_mute_is_authoritative() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-persistent");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    let listener_revision = listener.status().revision;
    let source_revision = mute(&mut source, false);
    agree(&mut listener, &mut source, false, false);
    assert!(listener.status().revision > listener_revision);
    assert!(source.status().revision > source_revision);

    // Persistent unmute has no lease, even with no additional GUI requests.
    std::thread::sleep(Duration::from_millis(350));
    agree(&mut listener, &mut source, false, false);
    ptt(&mut source, true);
    hold(&mut listener, &mut source, false, false);
    ptt(&mut source, false);
    std::thread::sleep(Duration::from_millis(350));
    agree(&mut listener, &mut source, false, false);

    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn source_gui_fresh_ptt_renews_listener_mic_until_release_and_can_press_again() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-ptt");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    // One GUI press spans multiple 250 ms leases; the Source core must renew it.
    hold(&mut listener, &mut source, false, true);
    ptt(&mut source, false);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    // Explicit mute revokes an outstanding talk grant as well as changing the base.
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, false);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[cfg(target_os = "linux")]
struct ResumeOnDrop<'a>(&'a common::Node);

#[cfg(target_os = "linux")]
impl Drop for ResumeOnDrop<'_> {
    fn drop(&mut self) {
        // Resume even while unwinding, before fixture teardown attempts graceful quit.
        let _ = self.0.resume();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn suspended_owned_source_expires_actual_listener_ptt_before_parent_link_timeout() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-expiry");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    hold(&mut listener, &mut source, false, true);
    assert_eq!(two.a.status().expect("Listener parent status").members, 1);
    assert!(audio::dry_launch(&two.b.root));
    assert_ne!(two.a.pid(), two.b.pid());

    // Only this fixture's pressing Source core is stopped. Its GUI connection stays
    // alive; dropping the connection would prove explicit revocation, not expiry.
    let resume = ResumeOnDrop(&two.b);
    let stopped = Instant::now();
    two.b
        .suspend()
        .expect("suspend owned isolated Dry Source core");
    let bound = Duration::from_millis(450); // 300 ms contract + 150 ms scheduling allowance.
    loop {
        let status = listener.status();
        let elapsed = stopped.elapsed();
        if microphone(&status, true, false) {
            eprintln!("Listener actual microphone lease expiry after Source STOP: {elapsed:?}");
            assert!(
                elapsed <= bound,
                "actual Listener expiry exceeded {bound:?}: {elapsed:?}"
            );
            break;
        }
        assert!(
            elapsed < bound,
            "Listener microphone did not expire: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        two.a
            .status()
            .expect("parent remains healthy at expiry")
            .members,
        1
    );
    // Keep the Source stopped for a full second: the surviving authenticated parent
    // must remain present after the native mic has already expired independently.
    while stopped.elapsed() < Duration::from_millis(1100) {
        let parent = two
            .a
            .status()
            .expect("Listener stays responsive during Source STOP");
        assert_eq!(
            parent.members, 1,
            "parent timed out before the one-second floor"
        );
        let actual = listener.status();
        assert!(
            microphone(&actual, true, false),
            "expired endpoint reactivated: {actual:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        two.a
            .status()
            .expect("parent still linked after one second")
            .members,
        1
    );
    drop(resume);
    ptt(&mut source, false);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn microphone_ptt_binding_collision_rejects_configuration_without_changing_endpoint() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-binding-collision");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    let before = listener.status();
    let mut config = before.config.clone().expect("Listener config");
    config
        .microphone
        .as_mut()
        .expect("microphone config")
        .ptt_hotkey = config::HotkeyBindings::default().emergency_release;
    match listener.request(gui_frame::Msg::SetAudio(wire::SetAudio {
        config: Some(config),
        expected_revision: before.revision,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok, "reserved emergency hotkey cannot become PTT");
            assert_eq!(result.error_code, "AUDIO_REVISION_CONFLICT");
        }
        other => panic!("SetAudio answered {other:?}"),
    }
    assert_eq!(listener.status().config, before.config);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[cfg(target_os = "linux")]
fn restart_listener_child(two: &Unpaired, listener: &mut Window) {
    let revision = listener.status().revision;
    let children = audio::audio_children(&two.a);
    assert_eq!(children.len(), 1, "one owned Listener audio child");
    let old_child = children[0];
    assert!(audio::dry_launch(&two.a.root));
    assert_ne!(old_child, two.a.pid());
    assert_ne!(old_child, two.b.pid());
    common::Node::kill_pid(old_child).expect("kill only owned isolated Dry Listener child");

    // Worker-loss status can be transient. Consume the authoritative push rather
    // than requiring a polling query to land before the replacement is ready.
    listener.event(|event| match event {
        gui_frame::Msg::AudioStatus(status)
            if status.revision > revision
                && status
                    .microphone
                    .as_ref()
                    .is_some_and(|mic| !mic.control_active && !mic.ptt_pressed) =>
        {
            Some(())
        }
        _ => None,
    });
    let deadline = Instant::now() + audio::TIMEOUT;
    loop {
        let children = audio::audio_children(&two.a);
        if children.len() == 1 && children[0] != old_child {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Listener did not spawn a fresh owned audio child: {children:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn killed_owned_listener_child_recovers_muted_without_replaying_held_source_ptt() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-child-restart");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    hold(&mut listener, &mut source, false, true);
    restart_listener_child(&two, &mut listener);
    // The original Source GUI connection remains held throughout the crash and
    // recovery. Old press renewals must never become a new grant on the new child.
    agree(&mut listener, &mut source, true, false);
    hold(&mut listener, &mut source, true, false);
    for node in [&two.a, &two.b] {
        let parent = node
            .status()
            .expect("input parent survives audio child crash");
        assert_eq!(parent.members, 1);
        assert_eq!(parent.agent, "ready");
    }
    ptt(&mut source, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    ptt(&mut source, false);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn revoking_source_mic_consent_remutes_actual_listener_without_dropping_parent() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-revoke-consent");
    mute(&mut source, false);
    agree(&mut listener, &mut source, false, false);
    let mut config = listener.status().config.expect("Listener config");
    config.peers[0].allow_mic_control = false;
    listener.configure(config);
    listener.wait(
        "revoked remote persistent unmute remutes actual endpoint",
        |status| microphone(status, true, false),
    );
    for node in [&two.a, &two.b] {
        let parent = node
            .status()
            .expect("input survives microphone consent revocation");
        assert_eq!(parent.members, 1);
        assert_eq!(parent.agent, "ready");
    }
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[cfg(target_os = "linux")]
#[test]
fn listener_child_restart_discards_remote_unmute_but_restores_explicit_local_unmute() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-origin-restart");
    mute(&mut source, false);
    agree(&mut listener, &mut source, false, false);
    restart_listener_child(&two, &mut listener);
    agree(&mut listener, &mut source, true, false);
    hold(&mut listener, &mut source, true, false);

    // Only an explicit local Listener decision can persist across child generations.
    mute(&mut listener, false);
    agree(&mut listener, &mut source, false, false);
    restart_listener_child(&two, &mut listener);
    agree(&mut listener, &mut source, false, false);
    hold(&mut listener, &mut source, false, false);
    for node in [&two.a, &two.b] {
        assert_eq!(
            node.status()
                .expect("input survives child replacement")
                .members,
            1
        );
    }
    mute(&mut listener, true);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn dropping_held_source_gui_remutes_actual_listener_before_parent_timeout() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-gui-drop");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    hold(&mut listener, &mut source, false, true);
    let closed = Instant::now();
    drop(source);
    let bound = Duration::from_millis(450);
    loop {
        let actual = listener.status();
        let elapsed = closed.elapsed();
        if microphone(&actual, true, false) {
            eprintln!("Listener actual microphone revoke after Source GUI drop: {elapsed:?}");
            assert!(
                elapsed <= bound,
                "GUI-close revoke exceeded {bound:?}: {elapsed:?}"
            );
            break;
        }
        assert!(
            elapsed < bound,
            "GUI-close revoke did not remute actual Listener: {actual:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    for node in [&two.a, &two.b] {
        let parent = node
            .status()
            .expect("GUI-close revoke preserves input parent");
        assert_eq!(parent.members, 1);
        assert_eq!(parent.agent, "ready");
    }
    let mut source = Window::open(&two.b);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn stale_unmute_does_not_cancel_an_existing_gui_ptt_hold() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-stale-unmute");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    assert!(source.status().revision > 0);
    match source.request(gui_frame::Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
        muted: false,
        expected_revision: 0,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok);
            assert_eq!(
                result.error_code,
                ipc::audio::codes::AUDIO_REVISION_CONFLICT
            );
        }
        other => panic!("stale unmute answered {other:?}"),
    }
    // Polling stays live beyond several native leases; rejection cannot cancel renewal.
    hold(&mut listener, &mut source, false, true);
    ptt(&mut source, false);
    agree(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[cfg(target_os = "linux")]
#[test]
fn failed_local_mute_persistence_keeps_the_native_endpoint_and_replacement_child_muted() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-failed-mute-save");
    let owner = std::cmp::max(two.a_id.as_str(), two.b_id.as_str());
    for node in [&two.a, &two.b] {
        node.wait_for(
            "settled ownership before holding config lock",
            audio::TIMEOUT,
            |status| status.serial > 0 && status.owner == owner,
        )
        .expect("ownership has settled");
    }
    mute(&mut listener, false);
    agree(&mut listener, &mut source, false, false);
    let path = two.a.root.join("config/config.toml");
    assert!(!config::Config::load(&path).unwrap().audio.microphone.muted);
    let lock = config::lock::FileLock::acquire(&path).expect("hold only this fixture's config");
    match listener.request(gui_frame::Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
        muted: true,
        expected_revision: 0,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok);
            assert_eq!(result.error_code, ipc::audio::codes::AUDIO_CONFIG_WRITE);
        }
        other => panic!("locked mute save answered {other:?}"),
    }
    agree(&mut listener, &mut source, true, false);
    assert!(listener.status().config.unwrap().microphone.unwrap().muted);
    assert!(!config::Config::load(&path).unwrap().audio.microphone.muted);
    drop(lock);
    restart_listener_child(&two, &mut listener);
    agree(&mut listener, &mut source, true, false);
    hold(&mut listener, &mut source, true, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[cfg(target_os = "linux")]
struct PausedAudioChild(u32);

#[cfg(target_os = "linux")]
impl PausedAudioChild {
    fn source(two: &Unpaired) -> Self {
        assert!(audio::dry_launch(&two.b.root));
        let children = audio::audio_children(&two.b);
        assert_eq!(children.len(), 1);
        let pid = children[0];
        assert_ne!(pid, two.a.pid());
        assert_ne!(pid, two.b.pid());
        assert!(std::process::Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .expect("pause only the owned Dry audio child")
            .success());
        Self(pid)
    }

    fn resume(&mut self) {
        assert!(std::process::Command::new("kill")
            .args(["-CONT", &self.0.to_string()])
            .status()
            .expect("resume the owned Dry audio child")
            .success());
        self.0 = 0;
    }
}

#[cfg(target_os = "linux")]
impl Drop for PausedAudioChild {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = std::process::Command::new("kill")
                .args(["-CONT", &self.0.to_string()])
                .status();
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn saturated_audio_gui_queue_latches_release_and_mute_despite_continued_polling() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-safety-admission");
    for mute_requested in [false, true] {
        if mute_requested {
            mute(&mut source, false);
            agree(&mut listener, &mut source, false, false);
        } else {
            mute(&mut source, true);
            agree(&mut listener, &mut source, true, false);
            ptt(&mut source, true);
            agree(&mut listener, &mut source, false, true);
        }
        let mut flood = Window::open(&two.b);
        let mut paused = PausedAudioChild::source(&two);
        // A real pending native enumeration holds the audio actor, not the KVM
        // session or GUI ingress. No injected queue-size/test-only production seam.
        for _ in 0..64 {
            flood.send(gui_frame::Msg::ListAudioDevices(wire::ListAudioDevices {
                kind: wire::AudioDeviceKind::Output as i32,
            }));
        }
        let barrier = flood.send(gui_frame::Msg::GetStatus(wire::GetStatus {}));
        let replies = flood.answers_through(barrier);
        assert!(
            replies.iter().any(|(_, message)| matches!(
                message,
                gui_frame::Msg::OpResult(result)
                    if !result.ok && result.error_code == ipc::audio::codes::AUDIO_UNAVAILABLE
            )),
            "normal audio queue must actually be saturated"
        );
        let safety = if mute_requested {
            gui_frame::Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
                muted: true,
                expected_revision: 0,
            })
        } else {
            gui_frame::Msg::SetPtt(wire::SetPtt {
                pressed: false,
                expected_revision: 0,
            })
        };
        let submitted = Instant::now();
        match source.request(safety) {
            gui_frame::Msg::OpResult(result) => {
                assert!(!result.ok);
                assert_eq!(result.error_code, ipc::audio::codes::AUDIO_UNAVAILABLE);
            }
            other => panic!("full-queue safety request answered {other:?}"),
        }
        paused.resume();
        drop(flood);
        let bound = Duration::from_millis(450);
        loop {
            let _ = source.request(gui_frame::Msg::GetStatus(wire::GetStatus {}));
            let actual = listener.status();
            if microphone(&actual, true, false) {
                assert!(submitted.elapsed() <= bound);
                break;
            }
            assert!(
                submitted.elapsed() < bound,
                "latched safety did not disarm: {actual:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The original pressing GUI remains connected and active. Its old hold
        // must not restart renewal after the queue drains.
        while submitted.elapsed() < Duration::from_millis(650) {
            let _ = source.request(gui_frame::Msg::GetStatus(wire::GetStatus {}));
            let actual = listener.status();
            assert!(
                microphone(&actual, true, false),
                "old hold reopened: {actual:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        agree(&mut listener, &mut source, true, false);
    }
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn another_gui_emergency_release_is_group_wide_idempotent_and_requires_a_fresh_press() {
    let _guard = common::serial();
    let (two, mut listener, mut source) = pair("audio-mic-release-all");
    mute(&mut source, true);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    let mut listener_tray = Window::open(&two.a);
    // This connection never pressed; the explicit action still releases the authority.
    ptt(&mut listener_tray, false);
    agree(&mut listener, &mut source, true, false);
    hold(&mut listener, &mut source, true, false);
    ptt(&mut listener_tray, false);
    agree(&mut listener, &mut source, true, false);
    ptt(&mut source, true);
    agree(&mut listener, &mut source, false, true);
    let mut source_tray = Window::open(&two.b);
    ptt(&mut source_tray, false);
    agree(&mut listener, &mut source, true, false);
    hold(&mut listener, &mut source, true, false);
    ptt(&mut source_tray, false);
    agree(&mut listener, &mut source, true, false);
    // Releasing talk grants is not an implicit persistent microphone mute.
    mute(&mut listener, false);
    agree(&mut listener, &mut source, false, false);
    ptt(&mut source_tray, false);
    agree(&mut listener, &mut source, false, false);
    hold(&mut listener, &mut source, false, false);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}
