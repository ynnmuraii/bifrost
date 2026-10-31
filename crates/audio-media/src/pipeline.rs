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
    status::AudioHealth,
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
    pub(crate) fn skip_frame(&mut self) {
        self.skip_frames(1);
    }
    pub(crate) fn skip_frames(&mut self, count: u64) {
        self.timestamp = self
            .timestamp
            .wrapping_add(count.wrapping_mul(u64::from(self.profile.frame_samples())) as u32);
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
                Ok(n * 2)
            }
        }
    }
}
