//! Only fixed lifecycle fields belong here. Never format an IPC frame, key, native error,
//! media title or PCM, even at DEBUG. Dependency diagnostics may contain MPRIS bodies.
use crate::{w, Msg};
use std::{io::IsTerminal, path::Path};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

fn filter(level: LevelFilter) -> EnvFilter {
    EnvFilter::new(format!("off,bifrost_audio={level}"))
}

pub(crate) fn init(logs_dir: Option<&Path>, generation: u64) {
    // Accept level names, not arbitrary target directives that could enable secret-bearing
    // dependency traces. The file uses the same 5 MiB / 5-file limits as the input agent.
    let level = std::env::var("BIFROST_LOG")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(LevelFilter::INFO);
    let sink = logs_dir.and_then(|dir| logging::file_sink(dir, "bifrost-audio", 5, 5));
    logging::init(
        filter(level),
        sink.map(logging::file_writer),
        std::io::stderr().is_terminal(),
    );
    if generation > 1 {
        tracing::info!(
            role = "audio",
            worker_generation = generation,
            pid = std::process::id(),
            "audio child restarted"
        );
    } else {
        tracing::info!(
            role = "audio",
            worker_generation = generation,
            pid = std::process::id(),
            "audio child started"
        );
    }
}

pub(crate) fn message(message: &Msg) {
    match message {
        Msg::Ready(ready) if ready.probed => {
            let available = ready
                .capabilities
                .iter()
                .filter(|cap| cap.availability == w::Availability::Available as i32)
                .count();
            tracing::info!(
                backend_kind = ready.backend,
                available,
                total = ready.capabilities.len(),
                "audio backend probe result"
            );
            for capability in &ready.capabilities {
                tracing::info!(
                    capability = capability.capability,
                    availability = capability.availability,
                    reason_code = capability.reason,
                    "audio backend capability"
                );
            }
        }
        Msg::SessionEvent(event) => {
            tracing::info!(session = ?event.session_id, event_kind = event.kind, stop_reason = event.reason, path_diagnostic = event.path_diagnostic, local_udp_port = event.local_udp_port, "audio session lifecycle");
        }
        Msg::BackendEvent(event) => {
            tracing::info!(
                event_kind = event.kind,
                direction = event.direction,
                "audio device event"
            );
        }
        Msg::Error(error) => {
            tracing::info!(code = error.code, "audio control rejected");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_logging_arguments_are_nonsecret_explicit_and_generation_is_nonzero() {
        let base = ["--ipc-endpoint", "local", "--token-file", "private-token"];
        let args = || {
            base.iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        let direct = crate::parse(args()).unwrap();
        assert_eq!(direct.worker_generation, 1);
        assert!(direct.logs_dir.is_none());
        let mut configured = args();
        configured.extend(["--logs-dir", "logs", "--worker-generation", "2"].map(str::to_owned));
        let configured = crate::parse(configured).unwrap();
        assert_eq!(configured.logs_dir.as_deref(), Some(Path::new("logs")));
        assert_eq!(configured.worker_generation, 2);
        for value in ["0", "-1", "not-a-generation"] {
            let mut invalid = args();
            invalid.extend(["--worker-generation", value].map(str::to_owned));
            assert!(crate::parse(invalid).is_err());
        }
    }

    #[test]
    fn debug_lifecycle_file_excludes_keys_launch_token_metadata_and_dependency_traces() {
        let dir = tempfile::tempdir().unwrap();
        let sink = logging::file_sink(dir.path(), "bifrost-audio", 5, 5).unwrap();
        let subscriber = logging::subscriber(
            filter(LevelFilter::DEBUG),
            Some(logging::file_writer(sink)),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            message(&Msg::Attach(w::Attach {
                launch_token: vec![b'k'; 32],
                ..Default::default()
            }));
            message(&Msg::StartSession(w::StartSession {
                forward: Some(w::DirectionMaterial {
                    key: vec![b'x'; 32],
                    nonce_prefix: vec![3; 4],
                }),
                reverse: Some(w::DirectionMaterial {
                    key: vec![b'y'; 32],
                    nonce_prefix: vec![4; 4],
                }),
                ..Default::default()
            }));
            message(&Msg::MediaSessions(w::MediaSessions {
                sessions: vec![w::MediaInfo {
                    title: "Dry media".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }));
            tracing::debug!(target: "zbus::message", title = "Dry media", "dependency metadata must not be logged");
            message(&Msg::Ready(w::Ready {
                probed: true,
                backend: 2,
                ..Default::default()
            }));
            message(&Msg::SessionEvent(w::SessionEvent {
                session_id: vec![7; 16],
                kind: 1,
                local_udp_port: 47903,
                ..Default::default()
            }));
            message(&Msg::SessionEvent(w::SessionEvent {
                session_id: vec![7; 16],
                kind: 5,
                reason: 5,
                path_diagnostic: 1,
                ..Default::default()
            }));
        });
        let bytes = std::fs::read(dir.path().join("bifrost-audio.log")).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("audio backend probe result"));
        assert!(text.contains("audio session lifecycle"));
        assert!(text.contains("path_diagnostic=1"));
        for byte in *b"kxy" {
            assert!(!bytes.windows(32).any(|part| part == [byte; 32]));
            assert!(!text.contains(&format!("{byte:02x}").repeat(32)));
            assert!(!text.contains(&format!("{byte:02X}").repeat(32)));
        }
        assert!(!text.contains("Dry media"));
        assert!(!text.contains("dependency metadata"));
    }
}
