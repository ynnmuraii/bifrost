//! Fixed media control payloads; no strings, acknowledgements or retransmitted audio.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverReport {
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub jitter_samples: u32,
    pub buffer_samples: u32,
}
impl ReceiverReport {
    pub const LEN: usize = 32;
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..8].copy_from_slice(&self.received.to_be_bytes());
        out[8..16].copy_from_slice(&self.lost.to_be_bytes());
        out[16..24].copy_from_slice(&self.late.to_be_bytes());
        out[24..28].copy_from_slice(&self.jitter_samples.to_be_bytes());
        out[28..].copy_from_slice(&self.buffer_samples.to_be_bytes());
        out
    }
    pub fn decode(input: &[u8]) -> Option<Self> {
        if input.len() != Self::LEN {
            return None;
        }
        Some(Self {
            received: u64::from_be_bytes(input[..8].try_into().ok()?),
            lost: u64::from_be_bytes(input[8..16].try_into().ok()?),
            late: u64::from_be_bytes(input[16..24].try_into().ok()?),
            jitter_samples: u32::from_be_bytes(input[24..28].try_into().ok()?),
            buffer_samples: u32::from_be_bytes(input[28..].try_into().ok()?),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_is_exact_width_big_endian() {
        let report = ReceiverReport {
            received: 1,
            lost: 2,
            late: 3,
            jitter_samples: 4,
            buffer_samples: 5,
        };
        let bytes = report.encode();
        assert_eq!(&bytes[..8], &1u64.to_be_bytes());
        assert_eq!(ReceiverReport::decode(&bytes), Some(report));
        assert!(ReceiverReport::decode(&bytes[..31]).is_none());
        assert!(ReceiverReport::decode(&[0; 33]).is_none());
    }
}
