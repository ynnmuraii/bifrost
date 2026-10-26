use std::io::Read;
use std::path::{Path, PathBuf};

use geometry::NodeId;
use protocol::{hello::HelloHandshake, limits, validate, wire};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{read_frame, write_frame, TransportError};

const MAX_PENDING: usize = 16;
const TTL_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_STORE_BYTES: u64 = 64 * 1024;

/// A persisted notice; the peer id is also its captured certificate SPKI pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingUnpairNotice {
    /// Certificate SPKI hash retained exclusively for delivering this notice.
    pub peer: NodeId,
    /// Original queue time, used for the thirty-day expiry.
    pub created_unix_secs: u64,
}

// Legacy CertificateUnknown is ambiguous: it may only suggest manual removal, never revoke.
pub(crate) fn unknown_peer_alert(error: &std::io::Error) -> bool {
    matches!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<rustls::Error>()),
        Some(rustls::Error::AlertReceived(
            rustls::AlertDescription::UnknownCA | rustls::AlertDescription::CertificateUnknown
        ))
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    peer: [u8; 32],
    created_unix_secs: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct PendingUnpairs {
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(default)]
    notices: Vec<Record>,
}

pub(crate) fn now_secs() -> u64 {
    u64::try_from(identity::now_unix_ms()).unwrap_or(0) / 1000
}

impl PendingUnpairs {
    pub(crate) fn discard_trusted(
        &mut self,
        pins: &std::collections::BTreeSet<NodeId>,
    ) -> Result<(), TransportError> {
        let mut next = self.clone();
        next.notices
            .retain(|record| !pins.contains(&NodeId::new(record.peer)));
        next.persist()?;
        *self = next;
        Ok(())
    }

    fn prune(&mut self, now: u64) {
        self.notices.retain(|record| {
            record.created_unix_secs <= now && now - record.created_unix_secs < TTL_SECS
        });
        self.notices
            .sort_by_key(|record| (record.created_unix_secs, record.peer));
        let mut seen = std::collections::BTreeSet::new();
        self.notices.retain(|record| seen.insert(record.peer));
        if self.notices.len() > MAX_PENDING {
            self.notices.drain(..self.notices.len() - MAX_PENDING);
        }
    }

    pub(crate) fn load(path: &Path, now: u64) -> Result<Self, TransportError> {
        let mut store = match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = String::new();
                file.take(MAX_STORE_BYTES + 1).read_to_string(&mut bytes)?;
                if bytes.len() as u64 > MAX_STORE_BYTES {
                    return Err(TransportError::Protocol(
                        "pending notice store is too large".into(),
                    ));
                }
                toml::from_str::<Self>(&bytes)
                    .map_err(|error| TransportError::Protocol(error.to_string()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        store.path = Some(path.to_owned());
        store.prune(now);
        store.persist()?;
        Ok(store)
    }

    fn persist(&self) -> Result<(), TransportError> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| TransportError::Protocol("pending notice path is not set".into()))?;
        let bytes =
            toml::to_string(self).map_err(|error| TransportError::Protocol(error.to_string()))?;
        identity::atomic::write_atomic(path, bytes.as_bytes())?;
        Ok(())
    }

    pub(crate) fn queue(
        &mut self,
        peer: NodeId,
        path: &Path,
        now: u64,
    ) -> Result<(), TransportError> {
        let mut next = self.clone();
        next.path = Some(path.to_owned());
        next.prune(now);
        next.notices
            .retain(|record| record.peer != *peer.as_bytes());
        // Evict the oldest existing notice, never the newly requested revocation.
        if next.notices.len() == MAX_PENDING {
            next.notices.remove(0);
        }
        next.notices.push(Record {
            peer: *peer.as_bytes(),
            created_unix_secs: now,
        });
        next.persist()?;
        *self = next;
        Ok(())
    }

    pub(crate) fn complete(&mut self, peer: NodeId, created: u64) -> Result<(), TransportError> {
        let mut next = self.clone();
        next.notices.retain(|record| {
            record.peer != *peer.as_bytes() || record.created_unix_secs != created
        });
        next.persist()?;
        *self = next;
        Ok(())
    }

    pub(crate) fn records(&self, now: u64) -> Vec<PendingUnpairNotice> {
        self.notices
            .iter()
            .filter(|record| {
                record.created_unix_secs <= now && now - record.created_unix_secs < TTL_SECS
            })
            .map(|record| PendingUnpairNotice {
                peer: NodeId::new(record.peer),
                created_unix_secs: record.created_unix_secs,
            })
            .collect()
    }
}

pub(crate) fn authenticated_notice(
    envelope: &wire::Envelope,
    sender: NodeId,
    connection: [u8; 16],
    notice: &wire::Unpaired,
) -> bool {
    envelope.major == limits::PROTOCOL_MAJOR
        && envelope.connection_id.as_slice() == connection
        && envelope.lease_id.is_empty()
        && notice.requester_node_id.as_slice() == sender.as_bytes()
        && notice.reason.len() <= 128
}

pub(crate) fn notice(local: NodeId) -> wire::Unpaired {
    wire::Unpaired {
        requester_node_id: local.as_bytes().to_vec(),
        reason: String::new(),
    }
}

/// No Link task, Up event, runtime dispatch or clipboard grant exists on this path.
pub(crate) async fn notify_only<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    peer: NodeId,
    local_hello: wire::Hello,
) -> Result<(), TransportError> {
    let remote = read_frame(stream).await?;
    validate::envelope(&remote).map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    let Some(wire::envelope::Payload::Hello(hello)) = &remote.payload else {
        return Err(TransportError::Protocol(
            "notify-only requires Hello".into(),
        ));
    };
    validate::hello(hello).map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    if hello.node_id.as_slice() != peer.as_bytes()
        || !hello
            .capabilities
            .contains(&(wire::Capability::UnpairNotice as i32))
        || !local_hello
            .capabilities
            .contains(&(wire::Capability::UnpairNotice as i32))
    {
        return Err(TransportError::Protocol(
            "notify-only identity or capability mismatch".into(),
        ));
    }
    let local: [u8; 32] = local_hello
        .node_id
        .as_slice()
        .try_into()
        .map_err(|_| TransportError::Protocol("invalid local Hello identity".into()))?;
    let mut handshake = HelloHandshake::acceptor(limits::PROTOCOL_MAJOR, limits::PROTOCOL_MINOR);
    let negotiated = handshake
        .on_remote_hello(&remote)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    let hello = handshake
        .local_hello(local_hello)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    write_frame(stream, &hello).await?;
    write_frame(
        stream,
        &wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            minor: negotiated.minor,
            connection_id: negotiated.connection_id.to_vec(),
            seq: 2,
            lease_id: Vec::new(),
            payload: Some(wire::envelope::Payload::Unpaired(notice(NodeId::new(
                local,
            )))),
        },
    )
    .await?;
    stream.shutdown().await?;
    // Drain without interpreting runtime frames so TCP close cannot discard the final notice.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(1000), async {
        let mut discard = [0u8; 1024];
        while tokio::io::AsyncReadExt::read(stream, &mut discard)
            .await
            .unwrap_or(0)
            != 0
        {}
    })
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_survives_restart_and_completion_is_persisted() {
        let directory = std::env::temp_dir()
            .join(format!("bifrost-unpair-{}", NodeId::new([7; 32])))
            .join(format!("{:x?}", identity::random16().unwrap()));
        let path = directory.join("pending.toml");
        let mut store = PendingUnpairs::load(&path, 100).unwrap();
        for byte in 0..20 {
            store
                .queue(NodeId::new([byte; 32]), &path, 100 + u64::from(byte))
                .unwrap();
        }
        let mut restarted = PendingUnpairs::load(&path, 120).unwrap();
        assert_eq!(restarted.records(120).len(), MAX_PENDING);
        restarted.complete(NodeId::new([19; 32]), 119).unwrap();
        let mut restarted = PendingUnpairs::load(&path, 120).unwrap();
        assert_eq!(restarted.records(120).len(), MAX_PENDING - 1);
        restarted
            .discard_trusted(&[NodeId::new([18; 32])].into())
            .unwrap();
        assert!(!PendingUnpairs::load(&path, 120)
            .unwrap()
            .records(120)
            .iter()
            .any(|notice| notice.peer == NodeId::new([18; 32])));
        assert!(PendingUnpairs::load(&path, 120 + TTL_SECS)
            .unwrap()
            .records(120 + TTL_SECS)
            .is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn hello(peer: NodeId, enabled: bool) -> wire::Hello {
        wire::Hello {
            node_id: peer.as_bytes().to_vec(),
            capabilities: if enabled {
                vec![wire::Capability::UnpairNotice as i32]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn notify_only_never_dispatches_runtime_or_grants_a_session() {
        let peer = NodeId::new([1; 32]);
        let local = NodeId::new([2; 32]);
        let (mut client, mut server) = tokio::io::duplex(16384);
        let task =
            tokio::spawn(async move { notify_only(&mut server, peer, hello(local, true)).await });
        let mut handshake =
            HelloHandshake::dialer(limits::PROTOCOL_MAJOR, limits::PROTOCOL_MINOR, [3; 16]);
        write_frame(
            &mut client,
            &handshake.local_hello(hello(peer, true)).unwrap(),
        )
        .await
        .unwrap();
        for (index, payload) in [
            wire::envelope::Payload::EnterCommit(wire::EnterCommit::default()),
            wire::envelope::Payload::Topology(wire::Topology::default()),
            wire::envelope::Payload::ClipboardGrant(wire::ClipboardGrant::default()),
        ]
        .into_iter()
        .enumerate()
        {
            write_frame(
                &mut client,
                &wire::Envelope {
                    major: limits::PROTOCOL_MAJOR,
                    connection_id: vec![3; 16],
                    seq: index as u64 + 2,
                    payload: Some(payload),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        client.shutdown().await.unwrap();
        let remote_hello = read_frame(&mut client).await.unwrap();
        handshake.on_remote_hello(&remote_hello).unwrap();
        let envelope = read_frame(&mut client).await.unwrap();
        let Some(wire::envelope::Payload::Unpaired(notice)) = &envelope.payload else {
            panic!("expected only an unpair notice");
        };
        assert!(authenticated_notice(&envelope, local, [3; 16], notice));
        assert_eq!(envelope.seq, 2);
        assert!(read_frame(&mut client).await.is_err());
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn notify_only_rejects_missing_capability_and_spoofed_hello() {
        let peer = NodeId::new([1; 32]);
        for (claimed, enabled) in [(peer, false), (NodeId::new([9; 32]), true)] {
            let (mut client, mut server) = tokio::io::duplex(16384);
            let task = tokio::spawn(async move {
                notify_only(&mut server, peer, hello(NodeId::new([2; 32]), true)).await
            });
            let mut handshake =
                HelloHandshake::dialer(limits::PROTOCOL_MAJOR, limits::PROTOCOL_MINOR, [3; 16]);
            write_frame(
                &mut client,
                &handshake.local_hello(hello(claimed, enabled)).unwrap(),
            )
            .await
            .unwrap();
            assert!(task.await.unwrap().is_err());
            assert!(read_frame(&mut client).await.is_err());
        }
    }

    #[test]
    fn only_unknown_pin_alert_classes_are_pair_rejection_evidence() {
        for alert in [
            rustls::AlertDescription::UnknownCA,
            // Older Bifrost verifiers mapped NotPinned through CertificateError::Other.
            rustls::AlertDescription::CertificateUnknown,
        ] {
            assert!(unknown_peer_alert(&std::io::Error::other(
                rustls::Error::AlertReceived(alert)
            )));
        }
        assert!(!unknown_peer_alert(&std::io::Error::other(
            rustls::Error::AlertReceived(rustls::AlertDescription::BadCertificate)
        )));
        assert!(!unknown_peer_alert(&std::io::Error::other(
            "handshake failed"
        )));
    }

    #[test]
    fn expiry_and_cap_are_deterministic() {
        let mut store = PendingUnpairs::default();
        for byte in 0..20 {
            store.notices.push(Record {
                peer: [byte; 32],
                created_unix_secs: 100 + u64::from(byte),
            });
        }
        store.prune(120);
        assert_eq!(store.records(120).len(), MAX_PENDING);
        assert_eq!(store.records(120)[0].peer, NodeId::new([4; 32]));
        store.prune(120 + TTL_SECS);
        assert!(store.records(120 + TTL_SECS).is_empty());
    }

    #[test]
    fn revocation_is_self_only_and_connection_bound() {
        let sender = NodeId::new([1; 32]);
        let mut envelope = wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            connection_id: vec![2; 16],
            ..Default::default()
        };
        let notice = notice(sender);
        assert!(authenticated_notice(&envelope, sender, [2; 16], &notice));
        assert!(!authenticated_notice(
            &envelope,
            NodeId::new([3; 32]),
            [2; 16],
            &notice
        ));
        assert!(!authenticated_notice(&envelope, sender, [4; 16], &notice));
        envelope.lease_id = vec![5; 16];
        assert!(!authenticated_notice(&envelope, sender, [2; 16], &notice));
    }
}
