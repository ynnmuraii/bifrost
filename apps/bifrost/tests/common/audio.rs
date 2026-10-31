//! Real GUI IPC helpers for isolated Dry-media integration tests.
//!
//! `BIFROST_TEST_AUDIO=1` makes existing Harness/Cluster fixtures start a Listener
//! and explicitly consenting Sources before KVM assertions. Explicit audio fixtures
//! instead mark only their isolated roots. Both are test-harness-only launch inputs:
//! production cores receive only the debug-only `--test-audio-dry` option.

use super::{gui_endpoint_of, Node, Unpaired};
use protocol::ipc::{self as wire, gui_frame};
use std::net::UdpSocket;
use std::path::Path;
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub fn enabled() -> bool {
    std::env::var_os("BIFROST_TEST_AUDIO").is_some_and(|value| value == "1")
}

pub fn mark_dry(root: &Path) {
    std::fs::create_dir_all(root.join("run")).expect("isolated run directory");
    std::fs::write(root.join("run/test-audio-dry"), []).expect("Dry launch marker");
}

pub fn dry_launch(root: &Path) -> bool {
    enabled() || root.join("run/test-audio-dry").is_file()
}

pub fn node_id(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len(), 64);
    (0..hex.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).expect("node ID hex"))
        .collect()
}

/// Reserve ports together until the configs have been assigned: loopback peers must
/// never both bind the production default 47903, or accidentally choose one port twice.
pub fn ports(count: usize) -> Vec<u16> {
    let sockets: Vec<_> = (0..count)
        .map(|_| loop {
            let socket = UdpSocket::bind("127.0.0.1:0").expect("reserve media UDP port");
            if ![47902, 47903].contains(&socket.local_addr().expect("UDP address").port()) {
                break socket;
            }
        })
        .collect();
    sockets
        .iter()
        .map(|socket| socket.local_addr().expect("UDP address").port())
        .collect()
}

/// One real GUI connection. Operation replies acknowledge requests, not media startup;
/// callers separately await authoritative status and worker observations.
pub struct Window {
    link: ipc::GuiLink<ipc::transport::ClientConnection>,
    runtime: tokio::runtime::Runtime,
    next_request: u64,
    events: Vec<gui_frame::Msg>,
}

impl Window {
    pub fn open(node: &Node) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("GUI runtime");
        let endpoint = gui_endpoint_of(&node.root);
        let deadline = Instant::now() + TIMEOUT;
        let connection = loop {
            match runtime.block_on(ipc::transport::connect(&endpoint)) {
                Ok(connection) => break connection,
                Err(error) => {
                    assert!(Instant::now() < deadline, "GUI connection: {error}");
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        };
        Self {
            link: ipc::GuiLink::new(connection),
            runtime,
            next_request: 1,
            events: Vec::new(),
        }
    }

    fn recv(&mut self, deadline: Instant) -> wire::GuiFrame {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.runtime
            .block_on(async { tokio::time::timeout(remaining, self.link.recv()).await })
            .expect("GUI frame before deadline")
            .expect("valid GUI frame")
    }

    pub fn send(&mut self, message: gui_frame::Msg) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        self.runtime
            .block_on(self.link.send(id, message))
            .expect("GUI request write");
        id
    }

    pub fn answer(&mut self, id: u64) -> gui_frame::Msg {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frame = self.recv(deadline);
            let message = frame.msg.expect("GUI message");
            if frame.request_id == id {
                return message;
            }
            assert_eq!(frame.request_id, 0, "unexpected request correlation");
            self.events.push(message);
        }
    }

    pub fn request(&mut self, message: gui_frame::Msg) -> gui_frame::Msg {
        let id = self.send(message);
        self.answer(id)
    }

    pub fn op(&mut self, message: gui_frame::Msg) {
        assert_ok(self.request(message));
    }

    pub fn event<T>(&mut self, select: impl Fn(&gui_frame::Msg) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(index) = self.events.iter().position(|event| select(event).is_some()) {
                return select(&self.events.remove(index)).expect("selected event");
            }
            let frame = self.recv(deadline);
            assert_eq!(frame.request_id, 0);
            self.events.push(frame.msg.expect("GUI event"));
        }
    }

    pub fn status(&mut self) -> wire::AudioStatus {
        match self.request(gui_frame::Msg::GetAudioStatus(wire::GetAudioStatus {})) {
            gui_frame::Msg::AudioStatus(status) => {
                ipc::audio::validate_status(&status).expect("authoritative valid audio status");
                status
            }
            other => panic!("GetAudioStatus answered {other:?}"),
        }
    }

    pub fn wait(
        &mut self,
        what: &str,
        predicate: impl Fn(&wire::AudioStatus) -> bool,
    ) -> wire::AudioStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = self.status();
            if predicate(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "{what}: {status:?}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn configure(&mut self, config: wire::AudioConfig) {
        let revision = self.status().revision;
        self.op(gui_frame::Msg::SetAudio(wire::SetAudio {
            config: Some(config),
            expected_revision: revision,
        }));
    }

    pub fn start(&mut self, peer: &[u8]) {
        let revision = self.status().revision;
        self.op(gui_frame::Msg::StartAudio(wire::StartAudio {
            peer_node_id: peer.to_vec(),
            kind: wire::AudioKind::System as i32,
            profile: wire::AudioProfile::Ethernet as i32,
            expected_revision: revision,
        }));
    }

    pub fn stop(&mut self, peer: &[u8], session_id: &[u8]) {
        self.op(gui_frame::Msg::StopAudio(wire::StopAudio {
            peer_node_id: peer.to_vec(),
            session_id: session_id.to_vec(),
            stop_all: false,
        }));
    }

    pub fn stopped(&mut self, session_id: &[u8]) {
        self.wait("exact media session disarms", |status| {
            status
                .sessions
                .iter()
                .filter(|session| session.session_id == session_id)
                .all(|session| {
                    matches!(
                        wire::AudioSessionState::try_from(session.state),
                        Ok(wire::AudioSessionState::Idle
                            | wire::AudioSessionState::AwaitConsent
                            | wire::AudioSessionState::Failed)
                    ) && session.capture_active != Some(true)
                        && session.sending != Some(true)
                })
        });
    }

    pub fn stop_all(&mut self) {
        self.op(gui_frame::Msg::StopAudio(wire::StopAudio {
            stop_all: true,
            ..Default::default()
        }));
    }

    pub fn running(&mut self, peer: &[u8]) -> wire::AudioSessionStatus {
        let status = self.wait("native media reaches Running", |status| {
            status.sessions.iter().any(|session| {
                session.peer == peer && session.state == wire::AudioSessionState::Running as i32
            })
        });
        status
            .sessions
            .into_iter()
            .find(|session| {
                session.peer == peer && session.state == wire::AudioSessionState::Running as i32
            })
            .expect("peer session")
    }

    pub fn closed(&mut self) {
        self.wait("all media sessions disarm", |status| {
            status.sessions.iter().all(|session| {
                matches!(
                    wire::AudioSessionState::try_from(session.state),
                    Ok(wire::AudioSessionState::Idle
                        | wire::AudioSessionState::AwaitConsent
                        | wire::AudioSessionState::Failed)
                ) && session.capture_active != Some(true)
                    && session.sending != Some(true)
            })
        });
    }
}

pub fn assert_ok(message: gui_frame::Msg) {
    match message {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "operation rejected: {result:?}"),
        other => panic!("expected operation reply, got {other:?}"),
    }
}

pub fn pair(two: &Unpaired, listener: &mut Window, source: &mut Window) {
    listener.op(gui_frame::Msg::StartPairing(wire::StartPairing {
        window_s: 120,
    }));
    let address = listener.event(|event| match event {
        gui_frame::Msg::PairingWindow(window) if window.open => Some(window.address.clone()),
        _ => None,
    });
    assert_eq!(address, format!("127.0.0.1:{}", two.a_port));
    source.op(gui_frame::Msg::PairWith(wire::PairWith { address }));
    let left = listener.event(|event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    let right = source.event(|event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    assert_eq!(left.sas_code, right.sas_code);
    assert_eq!(left.sas_code.len(), 6);
    assert!(left.sas_code.bytes().all(|digit| digit.is_ascii_digit()));
    let left_id = listener.send(gui_frame::Msg::PairingAnswer(wire::PairingAnswer {
        attempt_id: left.attempt_id,
        approve: true,
    }));
    let right_id = source.send(gui_frame::Msg::PairingAnswer(wire::PairingAnswer {
        attempt_id: right.attempt_id,
        approve: true,
    }));
    assert_ok(listener.answer(left_id));
    assert_ok(source.answer(right_id));
    for node in [&two.a, &two.b] {
        node.wait_for(
            "authenticated runtime link after pairing",
            TIMEOUT,
            |status| status.members == 1,
        )
        .expect("paired cores link");
    }
}

pub fn config(
    role: wire::AudioRole,
    port: u16,
    listener: &[u8],
    peers: &[Vec<u8>],
    consent: bool,
) -> wire::AudioConfig {
    wire::AudioConfig {
        enabled: true,
        role: role as i32,
        listener_node_id: listener.to_vec(),
        group_id: Vec::new(),
        mode: wire::AudioMode::MixAll as i32,
        udp_port: u32::from(port),
        profile: wire::AudioProfile::Ethernet as i32,
        output_device: "default".into(),
        output_fallback_to_default: true,
        capture_device: "default_monitor".into(),
        resume_system_audio: false,
        focus_debounce_ms: 300,
        focus_ramp_ms: 100,
        background_gain_db: -18,
        background_mute: false,
        microphone: Some(wire::AudioMicrophoneConfig {
            capture_device: "default".into(),
            muted: true,
            ..Default::default()
        }),
        peers: peers
            .iter()
            .map(|peer| wire::AudioPeerConfig {
                node_id: peer.clone(),
                allow_system_send: consent && role == wire::AudioRole::Source,
                allow_system_receive: consent && role == wire::AudioRole::Listener,
                volume_percent: 100,
                ..Default::default()
            })
            .collect(),
    }
}

/// Check actual worker observations and advancing authenticated media counters, not
/// successful command writes, model-only Running, or a fixed startup delay.
pub fn healthy(
    listener: &mut Window,
    source: &mut Window,
    listener_id: &[u8],
    source_id: &[u8],
) -> Vec<u8> {
    let receiving = listener.running(source_id);
    let sending = source.running(listener_id);
    assert_eq!(receiving.session_id, sending.session_id);
    assert_eq!(receiving.session_id.len(), 16);
    let local = receiving.consent.as_ref().expect("bilateral consent");
    assert!(
        local
            .local
            .as_ref()
            .expect("local consent")
            .allow_system_receive
    );
    assert!(
        local
            .remote
            .as_ref()
            .expect("remote consent")
            .allow_system_send
    );
    let remote = sending.consent.as_ref().expect("bilateral consent");
    assert!(
        remote
            .local
            .as_ref()
            .expect("local consent")
            .allow_system_send
    );
    assert!(
        remote
            .remote
            .as_ref()
            .expect("remote consent")
            .allow_system_receive
    );
    let id = receiving.session_id;
    let initial = receiving.health.expect("worker health").received_packets;
    listener.wait("authenticated media receive counters advance", |status| {
        status.sessions.iter().any(|session| {
            session.session_id == id
                && session.state == wire::AudioSessionState::Running as i32
                && session.health_state == wire::AudioStreamHealth::Ready as i32
                && session
                    .health
                    .as_ref()
                    .is_some_and(|health| health.received_packets > initial)
        })
    });
    source.wait("worker observes actual capture and send", |status| {
        status.sessions.iter().any(|session| {
            session.session_id == id
                && session.state == wire::AudioSessionState::Running as i32
                && session.capture_active == Some(true)
                && session.sending == Some(true)
        })
    });
    id
}

pub fn start_group(nodes: &[&Node]) {
    assert!(nodes.len() >= 2);
    for node in nodes {
        node.wait_for("core ready before Dry media setup", TIMEOUT, |status| {
            status.agent == "ready" && status.members >= nodes.len() - 1
        })
        .expect("Dry fixture links");
    }
    let ids: Vec<_> = nodes.iter().map(|node| node_id(&node.id)).collect();
    let ports = ports(nodes.len());
    let mut listener = Window::open(nodes[0]);
    listener.configure(config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        &ids[1..],
        true,
    ));
    let listener_status = listener.wait("Listener mints its group", |status| {
        status.enabled
            && status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id.len() == 16)
    });
    let group = listener_status.config.expect("Listener config").group_id;
    for index in 1..nodes.len() {
        let mut source = Window::open(nodes[index]);
        source.configure(config(
            wire::AudioRole::Source,
            ports[index],
            &ids[0],
            &ids[..1],
            true,
        ));
        let resolved = source.wait("Source resolves Listener group", |status| {
            status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id == group)
        });
        assert!(
            resolved.enabled,
            "authenticated resolution preserves explicit opt-in"
        );
        assert!(
            resolved.sessions.is_empty(),
            "resolution alone must not start media"
        );
        listener.start(&ids[index]);
        healthy(&mut listener, &mut source, &ids[0], &ids[index]);
    }
}

/// Disable through the GUI, then await actual worker and media-socket teardown.
pub fn disable(node: &Node, window: &mut Window) {
    let mut config = window.status().config.expect("live config");
    let port = u16::try_from(config.udp_port).expect("UDP port");
    config.enabled = false;
    window.configure(config);
    window.wait("authoritative disabled state", |status| !status.enabled);
    window.closed();
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + TIMEOUT;
        while !audio_children(node).is_empty() {
            assert!(Instant::now() < deadline, "disabled worker did not exit");
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_no_audio_resources(node, port);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (node, port);
}

#[cfg(target_os = "linux")]
pub fn audio_children(node: &Node) -> Vec<u32> {
    // Tokio may spawn on any worker thread. Linux exposes each thread's own
    // children here, not the whole thread group's children on the leader alone.
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir(format!("/proc/{}/task", node.pid)).expect("core thread list") {
        let path = task.expect("core thread").path().join("children");
        match std::fs::read_to_string(path) {
            Ok(list) => children.extend(
                list.split_whitespace()
                    .map(|pid| pid.parse::<u32>().expect("child PID")),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("core thread child list: {error}"),
        }
    }
    children
        .into_iter()
        .filter(|pid| match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(command) => command.split(|byte| *byte == 0).any(|arg| arg == b"audio"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("child argv: {error}"),
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub fn assert_single_audio_socket(node: &Node, media_port: u16) {
    let children = audio_children(node);
    assert_eq!(children.len(), 1, "one shared Listener audio child");
    let pid = children[0];
    let inodes: Vec<_> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("audio child FD list")
        .filter_map(|entry| {
            let path = match std::fs::read_link(entry.expect("FD entry").path()) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("reading audio child FD: {error}"),
            };
            path.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect();
    let mut bound = 0;
    for family in ["udp", "udp6"] {
        let sockets = std::fs::read_to_string(format!("/proc/{pid}/net/{family}"))
            .expect("audio child UDP table");
        for row in sockets.lines().skip(1) {
            let fields: Vec<_> = row.split_whitespace().collect();
            assert!(fields.len() >= 10, "UDP table row shape");
            if inodes.iter().any(|inode| inode == fields[9]) {
                let port = u16::from_str_radix(fields[1].rsplit(':').next().expect("UDP port"), 16)
                    .expect("hex port");
                assert_eq!(port, media_port, "shared worker uses configured media port");
                bound += 1;
            }
        }
    }
    assert_eq!(bound, 1, "both sessions share one bound media UDP socket");
}

/// Per-process FD ownership, not /proc/PID/net's namespace-wide socket listing.
#[cfg(target_os = "linux")]
pub fn assert_no_audio_resources(node: &Node, media_port: u16) {
    assert!(
        audio_children(node).is_empty(),
        "disabled core owns an audio child"
    );
    let socket_inodes: Vec<_> = std::fs::read_dir(format!("/proc/{}/fd", node.pid))
        .expect("core FD list")
        .filter_map(|entry| {
            let path = match std::fs::read_link(entry.expect("FD entry").path()) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("reading core FD ownership: {error}"),
            };
            path.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect();
    for family in ["udp", "udp6"] {
        let sockets =
            std::fs::read_to_string(format!("/proc/{}/net/{family}", node.pid)).expect("UDP table");
        for row in sockets.lines().skip(1) {
            let fields: Vec<_> = row.split_whitespace().collect();
            assert!(fields.len() >= 10, "UDP table row shape");
            if socket_inodes.iter().any(|inode| inode == fields[9]) {
                let port = u16::from_str_radix(fields[1].rsplit(':').next().expect("UDP port"), 16)
                    .expect("hex port");
                // The existing discovery socket is the only legitimate core UDP socket.
                assert_eq!(port, 47902, "disabled core owns non-discovery UDP {port}");
            }
        }
    }
    let _reservation =
        UdpSocket::bind(("127.0.0.1", media_port)).expect("disabled media port released");
}
