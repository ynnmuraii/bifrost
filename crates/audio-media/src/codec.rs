//! Bundled libopus (BSD-3-Clause), invoked on a worker, never in device callbacks.
use audio_model::{jitter::Profile, packet::MAX_PAYLOAD_LEN};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
#[derive(Debug)]
pub enum CodecError {
    Opus(opus::Error),
    InvalidFrame,
    InvalidPacket,
}
impl From<opus::Error> for CodecError {
    fn from(value: opus::Error) -> Self {
        Self::Opus(value)
    }
}
impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CodecError {}

pub struct OpusEncoder {
    inner: Encoder,
    samples: usize,
}
impl OpusEncoder {
    pub fn new(profile: Profile) -> Result<Self, CodecError> {
        let mut inner = Encoder::new(48_000, Channels::Stereo, Application::Audio)?;
        inner.set_bitrate(Bitrate::Bits(match profile {
            Profile::Ethernet => 96_000,
            Profile::Wifi => 64_000,
        }))?;
        inner.set_complexity(5)?;
        inner.set_force_channels(Some(Channels::Stereo))?;
        inner.set_inband_fec(profile == Profile::Wifi)?;
        inner.set_packet_loss_perc(if profile == Profile::Wifi { 10 } else { 0 })?;
        inner.set_dtx(false)?;
        Ok(Self {
            inner,
            samples: usize::from(profile.frame_samples()) * 2,
        })
    }
    pub fn lookahead_samples(&mut self) -> Result<u32, CodecError> {
        Ok(self.inner.get_lookahead()? as u32)
    }
    pub fn encode(
        &mut self,
        pcm: &[f32],
        output: &mut [u8; MAX_PAYLOAD_LEN],
    ) -> Result<usize, CodecError> {
        if pcm.len() != self.samples || pcm.iter().any(|x| !x.is_finite()) {
            return Err(CodecError::InvalidFrame);
        }
        Ok(self.inner.encode_float(pcm, output)?)
    }
}
pub struct OpusDecoder {
    inner: Decoder,
    frames: usize,
}
impl OpusDecoder {
    pub fn new(profile: Profile) -> Result<Self, CodecError> {
        Ok(Self {
            inner: Decoder::new(48_000, Channels::Stereo)?,
            frames: usize::from(profile.frame_samples()),
        })
    }
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.inner.reset_state()?;
        Ok(())
    }
    /// Empty input is exactly one negotiated frame of PLC. FEC input is the next packet.
    pub fn decode(
        &mut self,
        packet: &[u8],
        fec: bool,
        output: &mut [f32],
    ) -> Result<(), CodecError> {
        if output.len() != self.frames * 2 {
            return Err(CodecError::InvalidFrame);
        }
        if !packet.is_empty()
            && (packet.len() > MAX_PAYLOAD_LEN
                || self.inner.get_nb_samples(packet)? != self.frames
                || opus::packet::get_nb_channels(packet)? != Channels::Stereo)
        {
            return Err(CodecError::InvalidPacket);
        }
        if self.inner.decode_float(packet, output, fec)? != self.frames {
            return Err(CodecError::InvalidPacket);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_profiles_encode_decode_plc_and_fec() {
        for profile in [Profile::Ethernet, Profile::Wifi] {
            let mut encoder = OpusEncoder::new(profile).unwrap();
            let mut decoder = OpusDecoder::new(profile).unwrap();
            let mut pcm = [0.0; 1920];
            let len = usize::from(profile.frame_samples()) * 2;
            for (i, x) in pcm[..len].iter_mut().enumerate() {
                *x = (i as f32 * 0.03).sin() * 0.2;
            }
            let mut packet = [0; MAX_PAYLOAD_LEN];
            let n = encoder.encode(&pcm[..len], &mut packet).unwrap();
            let mut out = [0.0; 1920];
            decoder
                .decode(&packet[..n], false, &mut out[..len])
                .unwrap();
            decoder.decode(&[], false, &mut out[..len]).unwrap();
            decoder.decode(&packet[..n], true, &mut out[..len]).unwrap();
            assert!(out[..len].iter().all(|x| x.is_finite()));
            assert!(encoder.lookahead_samples().unwrap() <= 480);
            assert!(encoder.encode(&pcm[..len - 1], &mut packet).is_err());
        }
    }
    #[test]
    fn silk_fixture_contains_useful_inband_fec() {
        let mut encoder = OpusEncoder::new(Profile::Wifi).unwrap();
        encoder.inner.set_signal(opus::Signal::Voice).unwrap();
        encoder
            .inner
            .set_bandwidth(opus::Bandwidth::Wideband)
            .unwrap();
        let mut reference = OpusDecoder::new(Profile::Wifi).unwrap();
        let mut fec = OpusDecoder::new(Profile::Wifi).unwrap();
        let mut plc = OpusDecoder::new(Profile::Wifi).unwrap();
        let mut packets = Vec::new();
        let mut pcm = [0.0; 1920];
        for frame in 0..30 {
            for (i, pair) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let t = (frame * 960 + i) as f32 / 48_000.0;
                // A changing speech-like harmonic avoids a trivially predictable PLC fixture.
                let phase = std::f32::consts::TAU * (180.0 * t + 600.0 * t * t);
                let x = phase.sin() * 0.15 + (2.0 * phase).sin() * 0.07;
                pair.fill(x);
            }
            let mut packet = [0; MAX_PAYLOAD_LEN];
            let n = encoder.encode(&pcm, &mut packet).unwrap();
            packets.push(packet[..n].to_vec());
        }
        let mut expected = [0.0; 1920];
        let mut recovered = [0.0; 1920];
        let mut concealed = [0.0; 1920];
        for packet in &packets[..20] {
            reference.decode(packet, false, &mut expected).unwrap();
            fec.decode(packet, false, &mut recovered).unwrap();
            plc.decode(packet, false, &mut concealed).unwrap();
        }
        reference
            .decode(&packets[20], false, &mut expected)
            .unwrap();
        fec.decode(&packets[21], true, &mut recovered).unwrap();
        plc.decode(&[], false, &mut concealed).unwrap();
        let error = |output: &[f32]| {
            output
                .iter()
                .zip(expected)
                .map(|(&a, b)| f64::from(a - b).powi(2))
                .sum::<f64>()
        };
        assert!(
            error(&recovered) < error(&concealed),
            "SILK FEC error {} must be below PLC error {} (TOC={})",
            error(&recovered),
            error(&concealed),
            packets[21][0]
        );
        // The next packet remains available for normal decode at its own deadline.
        fec.decode(&packets[21], false, &mut recovered).unwrap();
    }
}
