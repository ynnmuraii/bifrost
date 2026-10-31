//! Bounded path observations shared by settings and the popup; never a firewall diagnosis.
use audio_model::status as model;
use protocol::ipc;
use ui_text::{fill_named, text, Key, Lang};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPathObservation {
    pub diagnostic: ipc::AudioPathDiagnostic,
    pub local_udp_port: u16,
    pub peer_udp_port: u16,
}

impl AudioPathObservation {
    pub fn from_wire(session: &ipc::AudioSessionStatus) -> Option<Self> {
        let diagnostic = ipc::AudioPathDiagnostic::try_from(session.path_diagnostic).ok()?;
        let failed = session.state == ipc::AudioSessionState::Failed as i32;
        let degraded = matches!(
            ipc::AudioSessionState::try_from(session.state).ok(),
            Some(
                ipc::AudioSessionState::Running
                    | ipc::AudioSessionState::Rebuffering
                    | ipc::AudioSessionState::PathValidation
                    | ipc::AudioSessionState::DevicePaused
            )
        ) && session.health_state == ipc::AudioStreamHealth::Degraded as i32;
        (failed || degraded && diagnostic != ipc::AudioPathDiagnostic::None).then_some(Self {
            diagnostic,
            local_udp_port: session.local_udp_port.try_into().ok()?,
            peer_udp_port: session.peer_udp_port.try_into().ok()?,
        })
    }

    pub fn from_model(session: &model::SessionStatus) -> Option<Self> {
        let diagnostic = match session.path_diagnostic {
            model::PathDiagnostic::None => ipc::AudioPathDiagnostic::None,
            model::PathDiagnostic::NoAuthenticatedPeerPackets => {
                ipc::AudioPathDiagnostic::NoAuthenticatedPeerPackets
            }
            model::PathDiagnostic::PeerProbesMissing => ipc::AudioPathDiagnostic::PeerProbesMissing,
            model::PathDiagnostic::ProbeResponsesMissing => {
                ipc::AudioPathDiagnostic::ProbeResponsesMissing
            }
            model::PathDiagnostic::AuthenticationFailures => {
                ipc::AudioPathDiagnostic::AuthenticationFailures
            }
        };
        let failed = session.state == model::SessionState::Failed;
        let degraded = matches!(
            session.state,
            model::SessionState::Running
                | model::SessionState::Rebuffering
                | model::SessionState::PathValidation
                | model::SessionState::DevicePaused
        ) && session.health_state == model::StreamHealth::Degraded;
        (failed || degraded && diagnostic != ipc::AudioPathDiagnostic::None).then_some(Self {
            diagnostic,
            local_udp_port: session.local_udp_port,
            peer_udp_port: session.peer_udp_port,
        })
    }

    pub fn has_diagnostic(&self) -> bool {
        self.diagnostic != ipc::AudioPathDiagnostic::None
    }

    pub fn reason(&self, lang: Lang, peer: &str) -> String {
        let key = match self.diagnostic {
            ipc::AudioPathDiagnostic::None => Key::AudioPathNone,
            ipc::AudioPathDiagnostic::NoAuthenticatedPeerPackets => Key::AudioPathNoPackets,
            ipc::AudioPathDiagnostic::PeerProbesMissing => Key::AudioPathPeerProbes,
            ipc::AudioPathDiagnostic::ProbeResponsesMissing => Key::AudioPathResponses,
            ipc::AudioPathDiagnostic::AuthenticationFailures => Key::AudioPathAuthentication,
        };
        fill_named(lang, key, &[("peer", &peer)])
    }

    pub fn ports(&self, lang: Lang, peer: &str) -> String {
        let port = |value: u16| {
            if value == 0 {
                text(lang, Key::Unknown2).to_owned()
            } else {
                value.to_string()
            }
        };
        fill_named(
            lang,
            Key::AudioPathPorts,
            &[
                ("peer", &peer),
                ("local", &port(self.local_udp_port)),
                ("remote", &port(self.peer_udp_port)),
            ],
        )
    }

    pub fn tooltip(&self, lang: Lang, peer: &str) -> String {
        format!("{}\n{}", self.reason(lang, peer), self.ports(lang, peer))
    }

    pub fn guidance(
        &self,
        lang: Lang,
        role: ipc::AudioRole,
        local: &str,
        peer: &str,
    ) -> Option<String> {
        let (port, listener, source) = match role {
            ipc::AudioRole::Listener => (self.local_udp_port, local, peer),
            ipc::AudioRole::Source => (self.peer_udp_port, peer, local),
            ipc::AudioRole::Unspecified => return None,
        };
        (self.has_diagnostic() && port != 0).then(|| {
            fill_named(
                lang,
                Key::AudioPathFirewall,
                &[
                    ("port", &port.to_string()),
                    ("listener", &listener),
                    ("source", &source),
                ],
            )
        })
    }
}
