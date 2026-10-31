//! Worker glue; the caller owns UDP pacing and a bounded decoded-PCM render ring.
//! The render device's consumed-frame clock supplies `now`; its PCM ring fill feeds drift.
use crate::{
    codec::{CodecError, OpusDecoder, OpusEncoder},
    crypto::{CryptoError, Opener, Sealer},
};
use audio_model::{
    drift::{CoarseCorrection, DriftController},
    jitter::{InsertError, JitterBuffer, Playout, Profile},
    packet::MAX_PAYLOAD_LEN,
    status::{AudioHealth, DriftDiagnostics, LatencyDiagnostics},
    Header, PacketKind,
};
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, Async, FixedAsync, PolynomialDegree, Resampler};
#[derive(Debug)]
pub enum MediaError {
    Crypto(CryptoError),
    Codec(CodecError),
    Jitter(InsertError),
    Resample,
    BufferTooSmall,
    InvalidFormat,
}
impl From<CryptoError> for MediaError {
    fn from(e: CryptoError) -> Self {
        Self::Crypto(e)
    }
}
impl From<CodecError> for MediaError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}
impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for MediaError {}

pub struct Sender {
    encoder: OpusEncoder,
    sealer: Sealer,
    profile: Profile,
    timestamp: u32,
    encoded: [u8; MAX_PAYLOAD_LEN],
}
impl Sender {
    pub fn new(profile: Profile, sealer: Sealer, first_timestamp: u32) -> Result<Self, MediaError> {
        Ok(Self {
            encoder: OpusEncoder::new(profile)?,
            sealer,
            profile,
            timestamp: first_timestamp,
            encoded: [0; MAX_PAYLOAD_LEN],
        })
    }
    /// Algorithmic delay queried from the configured Opus encoder, not a profile estimate.
    pub fn codec_lookahead_us(&mut self) -> Result<u32, MediaError> {
        Ok(
            (u64::from(self.encoder.lookahead_samples()?) * 1_000_000 / 48_000).min(2_000_000)
                as u32,
        )
    }
    /// One caller-paced frame. A failed send is never retried by this layer.
    pub fn encode(
        &mut self,
        now_ms: u64,
        pcm: &[f32],
        datagram: &mut [u8],
    ) -> Result<usize, MediaError> {
        let n = self.encoder.encode(pcm, &mut self.encoded)?;
        let timestamp = self.timestamp;
        self.timestamp = self
            .timestamp
            .wrapping_add(u32::from(self.profile.frame_samples()));
        Ok(self.sealer.seal(
            now_ms,
            PacketKind::Opus,
            timestamp,
            self.profile.frame_samples(),
            &self.encoded[..n],
            datagram,
        )?)
    }
    /// Control/probes share the same owned nonce counter as media packets.
    pub(crate) fn control(
        &mut self,
        now_ms: u64,
        kind: PacketKind,
        payload: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        self.sealer.seal(now_ms, kind, 0, 0, payload, output)
    }
}

/// At most one 20 ms input frame; septic interpolation adds only a few samples of delay.
/// This fast variant is for near-unity 48 kHz clock trimming, NOT arbitrary rate conversion
/// (no anti-alias filter). Native format conversion must use an appropriate filtered stage.
/// At unity ratio, the impulse fixture measures a 3-sample peak delay (0.0625 ms)
/// for 480/960-sample chunks; rubato conservatively reports 4 samples (0.0833 ms).
pub struct DriftResampler {
    inner: Async<f32>,
}
impl DriftResampler {
    pub fn new(frame_samples: usize) -> Result<Self, MediaError> {
        if !matches!(frame_samples, 480 | 960) {
            return Err(MediaError::InvalidFormat);
        }
        let inner = Async::new_poly(
            1.0,
            1.0011,
            PolynomialDegree::Septic,
            frame_samples,
            2,
            FixedAsync::Input,
        )
        .map_err(|_| MediaError::Resample)?;
        Ok(Self { inner })
    }
    pub fn output_capacity_frames(&self) -> usize {
        self.inner.output_frames_max()
    }
    pub fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }
    pub fn reset(&mut self) {
        self.inner.reset();
    }
    pub fn process(
        &mut self,
        pcm: &[f32],
        ratio: f64,
        output: &mut [f32],
    ) -> Result<usize, MediaError> {
        if pcm.len() != self.inner.input_frames_next() * 2
            || !ratio.is_finite()
            || !(0.999..=1.001).contains(&ratio)
        {
            return Err(MediaError::InvalidFormat);
        }
        if output.len() < self.output_capacity_frames() * 2 {
            return Err(MediaError::BufferTooSmall);
        }
        self.inner
            .set_resample_ratio(ratio, true)
            .map_err(|_| MediaError::Resample)?;
        let input =
            InterleavedSlice::new(pcm, 2, pcm.len() / 2).map_err(|_| MediaError::Resample)?;
        let frames = output.len() / 2;
        let mut output =
            InterleavedSlice::new_mut(output, 2, frames).map_err(|_| MediaError::Resample)?;
        let (_, n) = self
            .inner
            .process_into_buffer(&input, &mut output, None)
            .map_err(|_| MediaError::Resample)?;
        Ok(n)
    }
}

pub struct Receiver {
    opener: Opener,
    jitter: JitterBuffer,
    decoder: OpusDecoder,
    resampler: DriftResampler,
    drift: DriftController,
    coarse_corrections: u64,
    profile: Profile,
    render_target: u32,
    jitter_target: u32,
    encoded: [u8; MAX_PAYLOAD_LEN],
    pcm: [f32; 1920],
    concealed: u32,
    fade_in: bool,
    paused: bool,
}
impl Receiver {
    /// `render_target` is the caller's bounded decoded-PCM ring target, NOT jitter delay.
    pub fn new(profile: Profile, opener: Opener, render_target: u32) -> Result<Self, MediaError> {
        if render_target == 0 {
            return Err(MediaError::InvalidFormat);
        }
        Ok(Self {
            opener,
            jitter: JitterBuffer::new(profile),
            decoder: OpusDecoder::new(profile)?,
            resampler: DriftResampler::new(usize::from(profile.frame_samples()))?,
            drift: DriftController::new(render_target, profile.frame_samples()),
            coarse_corrections: 0,
            profile,
            render_target,
            jitter_target: profile.initial_delay(),
            encoded: [0; MAX_PAYLOAD_LEN],
            pcm: [0.0; 1920],
            concealed: 0,
            fade_in: true,
            paused: true,
        })
    }
    pub fn output_capacity_samples(&self) -> usize {
        self.resampler.output_capacity_frames() * 4
    }
    pub fn health(&self) -> AudioHealth {
        self.jitter.health()
    }
    pub fn drift_diagnostics(&self) -> DriftDiagnostics {
        DriftDiagnostics {
            ratio_ppm: self.drift.ratio_ppm().round() as i32,
            filtered_fill_error_frames: self.drift.filtered_fill_error_frames(),
            coarse_corrections: self.coarse_corrections,
        }
    }
    pub fn next_playout_deadline(&self) -> Option<u64> {
        self.jitter.next_deadline()
    }
    /// Encoded media and interpolation delay only; caller-owned PCM queues are separate.
    /// Jitter storage overlaps its scheduling target and must not be added to it.
    pub fn latency_diagnostics(&self) -> LatencyDiagnostics {
        LatencyDiagnostics {
            jitter_target_us: (u64::from(self.jitter.target_samples()) * 1_000_000 / 48_000)
                .min(2_000_000) as u32,
            jitter_buffer_us: (u64::from(self.jitter.buffered_samples()) * 1_000_000 / 48_000)
                .min(2_000_000) as u32,
            resampler_delay_us: (self.resampler.output_delay() as u64 * 1_000_000 / 48_000)
                .min(2_000_000) as u32,
            ..LatencyDiagnostics::default()
        }
    }
    pub fn target_delay_samples(&self) -> u32 {
        self.jitter.target_samples()
    }
    pub fn arrival_variation_samples(&self) -> f64 {
        self.jitter.arrival_variation_samples()
    }
    /// Authenticates all packet kinds; only Opus affects media health/jitter state.
    /// Returns authenticated nonmedia headers for a caller's bounded path-validation worker.
    pub fn receive(
        &mut self,
        now: u64,
        datagram: &mut [u8],
        installed: impl FnOnce(&Header) -> bool,
    ) -> Result<Header, MediaError> {
        let (header, payload) = self.opener.open(datagram, installed)?;
        if header.kind == PacketKind::Opus {
            if header.frame_samples != self.profile.frame_samples() {
                return Err(MediaError::InvalidFormat);
            }
            let discontinuities = self.jitter.health().discontinuities;
            self.jitter
                .insert(now, header.packet_number, header.media_timestamp, payload)
                .map_err(MediaError::Jitter)?;
            if self.jitter.health().discontinuities != discontinuities {
                self.decoder.reset()?;
                self.resampler.reset();
                self.drift.reset(self.render_target);
                self.fade_in = true;
                self.concealed = 0;
            }
        }
        Ok(header)
    }
    /// Call once per due codec frame on a worker. Return is per-channel output frames;
    /// append them to the caller's render ring. Pending/rebuffer returns zero, never blocks.
    /// `render_fill` is actual decoded-PCM ring occupancy, excluding encoded packet storage.
    pub fn playout(
        &mut self,
        now: u64,
        render_fill: f64,
        output: &mut [f32],
    ) -> Result<usize, MediaError> {
        if output.len() < self.output_capacity_samples() {
            return Err(MediaError::BufferTooSmall);
        }
        let result = self
            .jitter
            .pop(now, self.profile == Profile::Wifi, &mut self.encoded);
        let frame = usize::from(self.profile.frame_samples());
        match result {
            Playout::Pending => return Ok(0),
            Playout::Underrun => {
                if !self.paused {
                    self.decoder.reset()?;
                    self.resampler.reset();
                    self.drift.reset(self.render_target);
                }
                self.paused = true;
                self.fade_in = true;
                self.concealed = 0;
                return Ok(0);
            }
            Playout::Frame { len, .. } => {
                // Invalid authenticated codec frames conceal one frame, rather than poison PCM.
                if self
                    .decoder
                    .decode(&self.encoded[..len], false, &mut self.pcm[..frame * 2])
                    .is_err()
                {
                    self.decoder
                        .decode(&[], false, &mut self.pcm[..frame * 2])?;
                    self.concealed += frame as u32;
                    self.jitter.record_decode_loss();
                } else {
                    self.concealed = 0;
                }
            }
            Playout::Missing { fec_len, .. } => {
                if self
                    .decoder
                    .decode(
                        &self.encoded[..fec_len],
                        fec_len != 0,
                        &mut self.pcm[..frame * 2],
                    )
                    .is_err()
                {
                    self.decoder
                        .decode(&[], false, &mut self.pcm[..frame * 2])?;
                }
                self.concealed += frame as u32;
            }
        }
        self.paused = false;
        if self.concealed >= 5760 {
            self.jitter.reset();
            self.decoder.reset()?;
            self.resampler.reset();
            self.drift.reset(self.render_target);
            self.paused = true;
            self.fade_in = true;
            self.concealed = 0;
            return Ok(0);
        }
        if self.jitter_target != self.jitter.target_samples() {
            self.jitter_target = self.jitter.target_samples();
            self.drift.reset(self.render_target);
            self.fade_in = true;
        }
        if self.concealed != 0 {
            // Fade concealment progressively; do not indefinitely repeat stale audio.
            let gain = (1.0 - f64::from(self.concealed) / 5760.0).max(0.0) as f32;
            for x in &mut self.pcm[..frame * 2] {
                *x *= gain;
            }
        }
        if self.fade_in {
            for (i, pair) in self.pcm[..frame * 2]
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .enumerate()
            {
                let gain = (i as f32 / 240.0).min(1.0);
                pair[0] *= gain;
                pair[1] *= gain;
            }
            self.fade_in = false;
        }
        let update = self.drift.update(now / 48, render_fill);
        let n = self
            .resampler
            .process(&self.pcm[..frame * 2], update.ratio, output)?;
        match update.correction {
            CoarseCorrection::None => Ok(n),
            CoarseCorrection::DropFrame => {
                self.coarse_corrections = self.coarse_corrections.saturating_add(1);
                self.fade_in = true;
                Ok(0)
            }
            CoarseCorrection::DuplicateFrame => {
                // Crossfade the seam over 5 ms, keeping output bounded to two frames.
                output.copy_within(..n * 2, n * 2);
                let fade = 240.min(n);
                for i in 0..fade {
                    let gain = i as f32 / fade as f32;
                    for ch in 0..2 {
                        let previous = output[(n - fade + i) * 2 + ch];
                        output[(n + i) * 2 + ch] =
                            previous * (1.0 - gain) + output[(n + i) * 2 + ch] * gain;
                    }
                }
                self.coarse_corrections = self.coarse_corrections.saturating_add(1);
                Ok(n * 2)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::DirectionMaterial;
    use zeroize::Zeroizing;

    #[test]
    fn authenticated_controls_do_not_anchor_first_media_or_inflate_jitter() {
        let material = || DirectionMaterial::new(Zeroizing::new([42; 32]), Zeroizing::new([12; 4]));
        let mut sender = Sender::new(
            Profile::Ethernet,
            Sealer::new(material(), [8; 16], 0).unwrap(),
            98_765_431,
        )
        .unwrap();
        let mut receiver = Receiver::new(
            Profile::Ethernet,
            Opener::new(material(), [8; 16]).unwrap(),
            480,
        )
        .unwrap();
        let mut packet = [0; 1200];
        let mut output = vec![0.0; receiver.output_capacity_samples()];
        for ms in (0..500).step_by(10) {
            let n = sender
                .control(ms, PacketKind::Keepalive, &[1], &mut packet)
                .unwrap();
            receiver
                .receive(ms * 48, &mut packet[..n], |_| true)
                .unwrap();
            assert_eq!(receiver.playout(ms * 48, 0.0, &mut output).unwrap(), 0);
        }
        assert_eq!(receiver.latency_diagnostics().jitter_buffer_us, 0);
        for frame in 0..160 {
            let now = 24_000 + frame * 480;
            let pcm = if frame < 110 { [0.0; 960] } else { [0.1; 960] };
            let n = sender.encode(now / 48, &pcm, &mut packet).unwrap();
            receiver.receive(now, &mut packet[..n], |_| true).unwrap();
            receiver.playout(now, 480.0, &mut output).unwrap();
        }
        let health = receiver.health();
        assert_eq!(health.received_packets, 160);
        assert_eq!(health.lost_frames, 0);
        assert_eq!(health.late_frames, 0);
        assert_eq!(health.concealed_frames, 0);
        assert_eq!(health.discontinuities, 0);
        assert_eq!(health.underruns, 0);
        assert_eq!(receiver.target_delay_samples(), 1440);
        assert_eq!(receiver.drift_diagnostics().coarse_corrections, 0);
    }

    #[test]
    fn diagnostics_count_applied_drop_duplicate_and_survive_rebuffer() {
        let material = || DirectionMaterial::new(Zeroizing::new([42; 32]), Zeroizing::new([12; 4]));
        let mut sender = Sender::new(
            Profile::Ethernet,
            Sealer::new(material(), [8; 16], 0).unwrap(),
            0,
        )
        .unwrap();
        let mut receiver = Receiver::new(
            Profile::Ethernet,
            Opener::new(material(), [8; 16]).unwrap(),
            480,
        )
        .unwrap();
        let mut output = vec![0.0; receiver.output_capacity_samples()];
        let mut packet = [0; 1200];
        assert_eq!(receiver.playout(0, 5000.0, &mut output).unwrap(), 0);
        assert_eq!(receiver.drift_diagnostics().coarse_corrections, 0);
        let mut applied = 0;
        let mut drops = 0;
        let mut duplicates = 0;
        for frame in 0..100 {
            let now = frame * 480;
            let n = sender.encode(now / 48, &[0.0; 960], &mut packet).unwrap();
            receiver.receive(now, &mut packet[..n], |_| true).unwrap();
            let fill = if frame < 50 { 5000.0 } else { -1000.0 };
            let n = receiver.playout(now, fill, &mut output).unwrap();
            let count = receiver.drift_diagnostics().coarse_corrections;
            if count > applied {
                assert_eq!(count, applied + 1);
                if frame < 50 {
                    assert_eq!(n, 0);
                    drops += 1;
                } else {
                    assert!(n > 480);
                    duplicates += 1;
                }
                applied = count;
            }
        }
        assert!(drops > 0 && duplicates > 0);
        receiver.drift.reset(480);
        assert_eq!(receiver.drift_diagnostics().coarse_corrections, applied);
        for frame in 100..140 {
            receiver.playout(frame * 480, 480.0, &mut output).unwrap();
        }
        assert!(receiver.paused);
        let diagnostics = receiver.drift_diagnostics();
        assert_eq!(diagnostics.coarse_corrections, applied);
        assert_eq!(diagnostics.ratio_ppm, 0);
        assert_eq!(diagnostics.filtered_fill_error_frames, 0.0);
    }
}
