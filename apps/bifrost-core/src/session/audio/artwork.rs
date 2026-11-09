//! Session-scoped thumbnail delivery; no URLs, disk cache or general file transfer.
use super::*;
use std::sync::Arc;
const MAX_BYTES: usize = protocol::limits::MAX_AUDIO_ARTWORK_BYTES;
#[derive(Clone)]
struct Image {
    token: u64,
    revision: u64,
    id: [u8; 32],
    data: Arc<[u8]>,
    width: u32,
    height: u32,
}
struct Cached {
    view: fsm::SessionView,
    image: Image,
}
struct Assembly {
    view: fsm::SessionView,
    token: u64,
    id: [u8; 32],
    total: usize,
    data: Vec<u8>,
    width: u32,
    height: u32,
}
struct Validation {
    peer: NodeId,
    view: fsm::SessionView,
    token: u64,
    id: [u8; 32],
    admitted: bool,
    task: tokio::task::JoinHandle<Option<Image>>,
}
struct Sending {
    view: fsm::SessionView,
    token: u64,
    id: [u8; 32],
    offset: usize,
}
#[derive(Default)]
pub(super) struct State {
    native: Option<Image>,
    queued: BTreeMap<NodeId, Cached>,
    validation: Option<Validation>,
    received: BTreeMap<NodeId, Cached>,
    assembling: BTreeMap<NodeId, Assembly>,
    sending: BTreeMap<NodeId, Sending>,
    requested: BTreeMap<NodeId, (fsm::SessionView, u64, [u8; 32])>,
}
fn digest_matches(id: &[u8; 32], data: &[u8]) -> bool {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref() == id
}
fn current_identity_matches(
    current: Option<(u64, u64, [u8; 32])>,
    token: u64,
    id: [u8; 32],
) -> bool {
    current.is_some_and(|(current_token, _, current_id)| current_token == token && current_id == id)
}
// image's convenience JPEG decoder is permissive; use the same shipped decoder in strict mode.
fn valid_jpeg(data: &[u8], width: u32, height: u32) -> bool {
    use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    if data.is_empty()
        || data.len() > MAX_BYTES
        || !data.starts_with(&[255, 216])
        || !data.ends_with(&[255, 217])
        || !(1..=192).contains(&width)
        || !(1..=192).contains(&height)
    {
        return false;
    }
    let options = DecoderOptions::new_safe()
        .set_strict_mode(true)
        .set_max_width(192)
        .set_max_height(192)
        .jpeg_set_max_scans(16)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    if decoder.decode_headers().is_err()
        || decoder.dimensions() != Some((width as usize, height as usize))
    {
        return false;
    }
    let mut pixels = [0; 192 * 192 * 3];
    decoder
        .decode_into(&mut pixels[..width as usize * height as usize * 3])
        .is_ok()
}
fn validate_image(image: Image) -> Option<Image> {
    (digest_matches(&image.id, &image.data) && valid_jpeg(&image.data, image.width, image.height))
        .then_some(image)
}
impl Actor {
    pub(super) fn artwork_capable(&self, peer: NodeId) -> bool {
        self.media_scope(peer).is_some()
            && self.peers.get(&peer).is_some_and(|p| p.artwork_support)
            && self
                .links
                .borrow()
                .peers
                .get(&peer)
                .is_some_and(|p| p.artwork_support)
    }
    pub(super) fn artwork_available(&self, peer: NodeId, token: u64, id: Option<[u8; 32]>) -> bool {
        let Some(id) = id else {
            return false;
        };
        let Some(scope) = self.media_scope(peer) else {
            return false;
        };
        if !current_identity_matches(self.media_artwork_identity(peer), token, id) {
            return false;
        }
        if self.role() == model::AudioRole::Source {
            self.artwork
                .native
                .as_ref()
                .is_some_and(|image| image.token == token && image.id == id)
        } else {
            self.artwork_capable(peer)
                && self.artwork.received.get(&peer).is_some_and(|cached| {
                    cached.view == scope.view
                        && cached.image.token == token
                        && cached.image.id == id
                })
        }
    }
    pub(super) fn artwork_drop_transfer(&mut self, peer: NodeId) {
        self.artwork.received.remove(&peer);
        self.artwork.assembling.remove(&peer);
        self.artwork.queued.remove(&peer);
        if let Some(validation) = self
            .artwork
            .validation
            .as_mut()
            .filter(|validation| validation.peer == peer)
        {
            validation.admitted = false;
        }
        self.dirty = true;
    }
    pub(super) fn artwork_invalidate(&mut self, peer: NodeId) {
        self.artwork_drop_transfer(peer);
        self.artwork.received.remove(&peer);
        self.artwork.assembling.remove(&peer);
        self.artwork.sending.remove(&peer);
        self.artwork.requested.remove(&peer);
        if self.role() == model::AudioRole::Source {
            self.artwork.native = None;
        }
    }
    pub(super) fn artwork_on_worker(&mut self, raw_generation: u64, artwork: worker::MediaArtwork) {
        if self.role() != model::AudioRole::Source
            || self
                .worker
                .as_ref()
                .is_none_or(|worker| !worker.alive || worker.generation != raw_generation)
        {
            return;
        }
        let Ok(id) = artwork.artwork_id.as_slice().try_into() else {
            return;
        };
        if !self.records.keys().any(|peer| {
            self.media_artwork_identity(*peer)
                .is_some_and(|(token, revision, desired)| {
                    token == artwork.token && revision >= artwork.revision && desired == id
                })
        }) || artwork.data.is_empty()
            || artwork.data.len() > MAX_BYTES
            || !digest_matches(&id, &artwork.data)
        {
            return;
        }
        self.artwork.native = Some(Image {
            token: artwork.token,
            revision: artwork.revision,
            id,
            data: artwork.data.into(),
            width: artwork.width,
            height: artwork.height,
        });
        self.dirty = true;
    }
    pub(super) fn artwork_on_request(
        &mut self,
        parent: fsm::Parent,
        request: net::MediaArtworkRequest,
    ) {
        let peer = NodeId::new(parent.peer);
        if self.role() != model::AudioRole::Source || !self.artwork_capable(peer) {
            return;
        }
        let Some(scope) = self.media_scope(peer).filter(|scope| {
            scope.view.parent == parent && scope.view.session.as_slice() == request.session_id
        }) else {
            return;
        };
        let Some((token, _, id)) = self.media_artwork_identity(peer) else {
            return;
        };
        if request.token != token || request.artwork_id.as_slice() != id {
            return;
        }
        match self.artwork.sending.get(&peer) {
            Some(sending)
                if sending.view == scope.view
                    && sending.token == token
                    && sending.id == id
                    && self.artwork.native.as_ref().is_none_or(|image| {
                        image.token != token || image.id != id || sending.offset < image.data.len()
                    }) => {}
            _ => {
                self.artwork.sending.insert(
                    peer,
                    Sending {
                        view: scope.view,
                        token,
                        id,
                        offset: 0,
                    },
                );
            }
        }
    }
    pub(super) fn artwork_on_network(
        &mut self,
        parent: fsm::Parent,
        chunk: net::MediaArtworkChunk,
    ) {
        let peer = NodeId::new(parent.peer);
        if self.role() != model::AudioRole::Listener || !self.artwork_capable(peer) {
            return;
        }
        let Some(scope) = self.media_scope(peer).filter(|scope| {
            scope.view.parent == parent && scope.view.session.as_slice() == chunk.session_id
        }) else {
            return;
        };
        let Some((token, revision, id)) = self.media_artwork_identity(peer) else {
            return;
        };
        if token != chunk.token || revision < chunk.revision || id.as_slice() != chunk.artwork_id {
            return;
        }
        if self.artwork_available(peer, token, Some(id)) {
            return;
        }
        if chunk.offset == 0 {
            self.artwork.assembling.insert(
                peer,
                Assembly {
                    view: scope.view,
                    token,
                    id,
                    total: chunk.total_bytes as usize,
                    data: Vec::with_capacity(chunk.total_bytes as usize),
                    width: chunk.width,
                    height: chunk.height,
                },
            );
        }
        let Some(assembly) = self.artwork.assembling.get_mut(&peer) else {
            return;
        };
        if assembly.view != scope.view
            || assembly.token != token
            || assembly.id != id
            || assembly.total != chunk.total_bytes as usize
            || assembly.width != chunk.width
            || assembly.height != chunk.height
            || assembly.data.len() != chunk.offset as usize
            || assembly.data.len().saturating_add(chunk.data.len()) > assembly.total
        {
            return;
        }
        assembly.data.extend_from_slice(&chunk.data);
        if assembly.data.len() == assembly.total {
            let assembly = self
                .artwork
                .assembling
                .remove(&peer)
                .expect("complete assembly");
            self.artwork.queued.insert(
                peer,
                Cached {
                    view: scope.view,
                    image: Image {
                        token,
                        revision: chunk.revision,
                        id,
                        data: assembly.data.into(),
                        width: assembly.width,
                        height: assembly.height,
                    },
                },
            );
            self.dirty = true;
        }
    }
    pub(super) async fn artwork_on_gui(&mut self, request: Request) {
        let gui::gui_frame::Msg::GetAudioArtwork(query) = &request.message else {
            return;
        };
        let found = self.records.iter().find_map(|(peer, record)| {
            let id: [u8; 32] = query.artwork_id.as_slice().try_into().ok()?;
            if record.view.session.as_slice() != query.session_id
                || !self.artwork_available(*peer, query.session_token, Some(id))
            {
                return None;
            }
            let image = if self.role() == model::AudioRole::Source {
                self.artwork.native.as_ref()?
            } else {
                &self.artwork.received.get(peer)?.image
            };
            Some(gui::AudioArtwork {
                session_id: record.view.session.to_vec(),
                session_token: image.token,
                artwork_id: image.id.to_vec(),
                mime: "image/jpeg".into(),
                data: image.data.to_vec(),
                width: image.width,
                height: image.height,
            })
        });
        if let Some(artwork) = found {
            answer(&request, gui::gui_frame::Msg::AudioArtwork(artwork)).await;
        } else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
        }
    }
    pub(super) async fn artwork_tick(&mut self) {
        if self
            .artwork
            .validation
            .as_ref()
            .is_some_and(|validation| validation.task.is_finished())
        {
            let validation = self.artwork.validation.take().expect("finished validation");
            if let Ok(Some(image)) = validation.task.await {
                if validation.admitted
                    && self.artwork_capable(validation.peer)
                    && self
                        .media_scope(validation.peer)
                        .is_some_and(|scope| scope.view == validation.view)
                    && current_identity_matches(
                        self.media_artwork_identity(validation.peer),
                        validation.token,
                        validation.id,
                    )
                {
                    self.artwork.received.insert(
                        validation.peer,
                        Cached {
                            view: validation.view,
                            image,
                        },
                    );
                    self.dirty = true;
                }
            }
        }
        if self.artwork.validation.is_none() {
            if let Some((peer, Cached { view, image })) = self.artwork.queued.pop_first() {
                if self.artwork_capable(peer)
                    && self
                        .media_scope(peer)
                        .is_some_and(|scope| scope.view == view)
                    && current_identity_matches(
                        self.media_artwork_identity(peer),
                        image.token,
                        image.id,
                    )
                {
                    let (token, id) = (image.token, image.id);
                    self.artwork.validation = Some(Validation {
                        peer,
                        view,
                        token,
                        id,
                        admitted: true,
                        task: tokio::task::spawn_blocking(move || validate_image(image)),
                    });
                }
            }
        }
        let mut peers = [None; model::MAX_AUDIO_SESSIONS];
        for (slot, peer) in peers.iter_mut().zip(self.records.keys()) {
            *slot = Some(*peer);
        }
        for peer in peers.into_iter().flatten() {
            let desired = self.media_artwork_identity(peer);
            let view = self.media_scope(peer).map(|scope| scope.view);
            let capable = self.artwork_capable(peer);
            self.artwork.received.retain(|node, cached| {
                *node != peer
                    || (capable
                        && view == Some(cached.view)
                        && desired.is_some_and(|(token, _, id)| {
                            token == cached.image.token && id == cached.image.id
                        }))
            });
            self.artwork.assembling.retain(|node, assembly| {
                *node != peer
                    || (capable
                        && view == Some(assembly.view)
                        && desired.is_some_and(|(token, _, id)| {
                            token == assembly.token && id == assembly.id
                        }))
            });
            if !capable {
                self.artwork.sending.remove(&peer);
                self.artwork.requested.remove(&peer);
                continue;
            }
            if self.role() == model::AudioRole::Listener {
                if let (Some(view), Some((token, _, id))) = (view, desired) {
                    let identity = (view, token, id);
                    if !self.artwork_available(peer, token, Some(id))
                        && self.artwork.requested.get(&peer) != Some(&identity)
                    {
                        let message = net::AudioControl {
                            control_version: 1,
                            body: Some(net::audio_control::Body::ArtworkRequest(
                                net::MediaArtworkRequest {
                                    session_id: view.session.to_vec(),
                                    token,
                                    artwork_id: id.to_vec(),
                                },
                            )),
                        };
                        if self
                            .manager
                            .send_audio_control(&peer, message)
                            .await
                            .is_ok()
                        {
                            self.artwork.requested.insert(peer, identity);
                        }
                    }
                } else {
                    self.artwork.requested.remove(&peer);
                }
                continue;
            }
            let Some((token, _, id)) = desired else {
                self.artwork.sending.remove(&peer);
                self.artwork.native = None;
                continue;
            };
            let Some(image) = self
                .artwork
                .native
                .as_ref()
                .filter(|image| image.token == token && image.id == id)
            else {
                continue;
            };
            let Some(view) = view else {
                continue;
            };
            let Some(sending) = self.artwork.sending.get_mut(&peer) else {
                continue;
            };
            if sending.view != view || sending.token != token || sending.id != id {
                self.artwork.sending.remove(&peer);
                continue;
            }
            if sending.offset >= image.data.len() {
                continue;
            }
            let offset = sending.offset;
            let end = (offset + protocol::limits::MAX_AUDIO_ARTWORK_CHUNK).min(image.data.len());
            let message = net::AudioControl {
                control_version: 1,
                body: Some(net::audio_control::Body::Artwork(net::MediaArtworkChunk {
                    session_id: view.session.to_vec(),
                    token,
                    revision: image.revision,
                    artwork_id: id.to_vec(),
                    total_bytes: image.data.len() as u32,
                    offset: offset as u32,
                    data: image.data[offset..end].to_vec(),
                    width: image.width,
                    height: image.height,
                })),
            };
            if self
                .manager
                .send_audio_control(&peer, message)
                .await
                .is_ok()
            {
                if let Some(sending) = self.artwork.sending.get_mut(&peer) {
                    sending.offset = end;
                }
            }
            // One thumbnail chunk per actor tick, behind input and audio authorization traffic.
            break;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([36, 72, 144]));
        let mut data = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut data)
            .encode_image(&image)
            .unwrap();
        data
    }
    #[test]
    fn receiver_requires_complete_real_jpeg_and_actual_bounded_dimensions() {
        let valid = jpeg(192, 96);
        assert!(valid_jpeg(&valid, 192, 96));
        assert!(!valid_jpeg(&valid, 1, 1));
        let oversized = jpeg(193, 1);
        assert!(!valid_jpeg(&oversized, 1, 1));
        let mut truncated = valid[..valid.len() / 2].to_vec();
        truncated.extend_from_slice(&[255, 217]);
        assert!(!valid_jpeg(&truncated, 192, 96));
        assert!(!valid_jpeg(&valid[..valid.len() - 2], 192, 96));
        let mut malformed_progressive = valid.clone();
        let sof = malformed_progressive
            .windows(2)
            .position(|bytes| bytes == [255, 192])
            .unwrap();
        malformed_progressive[sof + 1] = 194;
        assert!(!valid_jpeg(&malformed_progressive, 192, 96));
    }
    #[test]
    fn matching_hash_does_not_make_invalid_receiver_bytes_cacheable() {
        let valid = jpeg(32, 32);
        let mut truncated = valid[..valid.len() / 2].to_vec();
        truncated.extend_from_slice(&[255, 217]);
        for (data, width, height) in [
            (jpeg(193, 1), 1, 1),
            (truncated, 32, 32),
            (valid.clone(), 1, 1),
        ] {
            let id = ring::digest::digest(&ring::digest::SHA256, &data)
                .as_ref()
                .try_into()
                .unwrap();
            let image = Image {
                token: 1,
                revision: 1,
                id,
                data: data.into(),
                width,
                height,
            };
            assert!(validate_image(image).is_none());
        }
        let id = ring::digest::digest(&ring::digest::SHA256, &valid)
            .as_ref()
            .try_into()
            .unwrap();
        let data: Arc<[u8]> = valid.into();
        let image = validate_image(Image {
            token: 1,
            revision: 1,
            id,
            data: data.clone(),
            width: 32,
            height: 32,
        })
        .unwrap();
        assert_eq!(image.id, id);
        assert!(Arc::ptr_eq(&image.data, &data));
    }
    #[test]
    fn lookup_tracks_current_token_hash_immediately_not_the_cleanup_tick_or_position_revision() {
        let a = [1; 32];
        let b = [2; 32];
        assert!(current_identity_matches(Some((7, 1, a)), 7, a));
        assert!(current_identity_matches(Some((7, 99, a)), 7, a));
        assert!(!current_identity_matches(Some((7, 2, b)), 7, a));
        assert!(!current_identity_matches(Some((8, 2, a)), 7, a));
        assert!(!current_identity_matches(None, 7, a));
    }

    #[tokio::test]
    async fn completed_artwork_request_requeues_without_restarting_partial_transfer() {
        let (_root, coordinator, mut actor, peer, parent) =
            sharing::tests::lifecycle::listener_fixture();
        actor.peers.get_mut(&peer).unwrap().artwork_support = true;
        coordinator
            .links
            .send_modify(|links| links.peers = actor.peers.clone());
        actor.media_tick().await;
        let data = jpeg(32, 32);
        let id: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, &data)
            .as_ref()
            .try_into()
            .unwrap();
        let view = actor.records[&peer].view;
        actor
            .media_on_network(
                parent,
                &net::AudioControl {
                    control_version: 1,
                    body: Some(net::audio_control::Body::MediaInfo(net::MediaInfo {
                        session_id: view.session.to_vec(),
                        present: true,
                        token: 11,
                        revision: 12,
                        app: "owned-player".into(),
                        title: "Owned paused track".into(),
                        playback: net::MediaPlayback::Paused as i32,
                        commands: 1,
                        allow_control: true,
                        artwork_id: id.to_vec(),
                        ..Default::default()
                    })),
                },
            )
            .await;
        actor.artwork_on_network(
            parent,
            net::MediaArtworkChunk {
                session_id: view.session.to_vec(),
                token: 11,
                revision: 12,
                artwork_id: id.to_vec(),
                total_bytes: data.len() as u32,
                offset: 0,
                data: data.clone(),
                width: 32,
                height: 32,
            },
        );
        timeout(Duration::from_secs(2), async {
            loop {
                actor.artwork_tick().await;
                if actor.media_info(peer).unwrap().artwork_id == Some(id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("consumer receives the validated JPEG");
        actor.feed(fsm::Input::Buffering(view)).unwrap();
        actor.media_tick().await;
        actor.artwork_tick().await;
        let buffering = actor.records[&peer].view;
        actor.feed(fsm::Input::PathReady(buffering)).unwrap();
        actor.media_tick().await;
        assert_eq!(actor.media_info(peer).unwrap().artwork_id, None);
        assert!(actor.media_allowed(peer));

        // Re-establish the Source side through the model's accepted Hello/consent seam.
        actor.config.role = config::AudioRole::Source;
        actor.config.listener_node_id = Some(config::audio::HexId(*peer.as_bytes()));
        actor.config.allow_send_to = vec![config::audio::HexId(*peer.as_bytes())];
        actor.model = fsm::ControlPlane::new(*actor.local.as_bytes());
        actor.configure().unwrap();
        let remote = actor.peers.get_mut(&peer).unwrap();
        remote.support.system_source = false;
        remote.support.listener = true;
        let support = remote.support;
        let hello = remote.hello.as_mut().unwrap();
        hello.role = model::AudioRole::Listener;
        hello.listener = *peer.as_bytes();
        let hello = *hello;
        let mut offer = actor.records[&peer].agreement.offer;
        offer.listener = *peer.as_bytes();
        offer.source = *actor.local.as_bytes();
        offer.consent_revision = hello.consent_revision;
        actor.effects.clear();
        for input in [
            fsm::Input::WorkerUp {
                generation: view.worker_generation,
                capture: true,
                render: false,
            },
            fsm::Input::PeerUp {
                parent,
                support,
                trusted: true,
                local_allowed: true,
                consent_revision: actor.consent_revision,
            },
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Hello(hello),
            },
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Offer(offer),
            },
            fsm::Input::SourceNonce {
                parent,
                session: view.session,
                nonce: [9; 32],
            },
        ] {
            actor.feed(input).unwrap();
        }
        let agreement = actor
            .effects
            .iter()
            .find_map(|effect| match effect {
                fsm::Effect::DeriveKeys { agreement, .. } => Some(*agreement),
                _ => None,
            })
            .expect("real source model agreement");
        let source_view = actor.model.session(*peer.as_bytes()).unwrap();
        for input in [
            fsm::Input::KeysReady {
                view: source_view,
                transcript_sha256: [10; 32],
            },
            fsm::Input::ChildPrepared(source_view),
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Commit {
                    session: view.session,
                    transcript_sha256: [10; 32],
                },
            },
            fsm::Input::ChildStartIssued(source_view),
            fsm::Input::ChildStarted(source_view),
            fsm::Input::PathReady(source_view),
        ] {
            actor.feed(input).unwrap();
        }
        let record = actor.records.get_mut(&peer).unwrap();
        record.role = model::AudioRole::Source;
        record.agreement = agreement;
        assert_eq!(record.view, view);
        actor.effects.clear();
        coordinator
            .links
            .send_modify(|links| links.peers = actor.peers.clone());
        actor.ready = Some(worker::Ready {
            backend: worker::BackendKind::Dry as i32,
            probed: true,
            capabilities: vec![worker::CapabilityAvailability {
                capability: worker::Capability::MediaSessions as i32,
                availability: worker::Availability::Available as i32,
                reason: 0,
            }],
        });
        actor.artwork_on_worker(
            1,
            worker::MediaArtwork {
                token: 11,
                revision: 12,
                artwork_id: id.to_vec(),
                data: data.clone(),
                width: 32,
                height: 32,
            },
        );
        assert!(actor.artwork_available(peer, 11, Some(id)));
        let request = || net::MediaArtworkRequest {
            session_id: view.session.to_vec(),
            token: 11,
            artwork_id: id.to_vec(),
        };
        actor.artwork_on_request(parent, request());
        assert_eq!(actor.artwork.sending[&peer].offset, 0);
        actor.artwork.sending.get_mut(&peer).unwrap().offset = 1;
        actor.artwork_on_request(parent, request());
        assert_eq!(
            actor.artwork.sending[&peer].offset, 1,
            "duplicate cannot restart an in-flight transfer"
        );
        actor.artwork.sending.get_mut(&peer).unwrap().offset = data.len();
        actor.artwork_on_request(parent, request());
        assert_eq!(
            actor.artwork.sending[&peer].offset, 0,
            "a completed image is resendable"
        );
        actor.artwork.sending.get_mut(&peer).unwrap().offset = data.len();
        let mut stale = parent;
        stale.generation = [99; 16];
        actor.artwork_on_request(stale, request());
        assert_eq!(actor.artwork.sending[&peer].offset, data.len());
        actor.config.system_sharing = Some(false);
        actor.artwork_on_request(parent, request());
        assert_eq!(
            actor.artwork.sending[&peer].offset,
            data.len(),
            "withdrawn consent cannot schedule a resend"
        );
        actor.media_tick().await;
        actor.artwork_tick().await;
        assert!(!actor.artwork.sending.contains_key(&peer));
        assert!(actor.media_info(peer).is_none());
        assert!(!actor.media_allowed(peer));
    }
}
