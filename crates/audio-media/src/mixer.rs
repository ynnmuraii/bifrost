//! Three-input stereo mix point, independent of session/network ownership.
pub const MAX_INPUTS: usize = 3;
#[derive(Clone, Copy)]
struct Gain {
    current: f32,
    target: f32,
    step: f32,
    remaining: usize,
}
impl Gain {
    fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}
const UNITY: Gain = Gain {
    current: 1.0,
    target: 1.0,
    step: 0.0,
    remaining: 0,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixError {
    InvalidGain,
    InvalidInput,
}
pub struct Mixer {
    gains: [Gain; MAX_INPUTS],
    ramp_frames: usize,
    limited_frames: u64,
}
impl Mixer {
    pub fn new(ramp_frames: usize) -> Self {
        Self {
            gains: [UNITY; MAX_INPUTS],
            ramp_frames: ramp_frames.max(1),
            limited_frames: 0,
        }
    }
    pub fn limited_frames(&self) -> u64 {
        self.limited_frames
    }
    pub(crate) fn reset_input(&mut self, input: usize) {
        self.gains[input] = UNITY;
    }
    /// Listener-local gain only. No writes to any source/native master volume.
    pub fn set_gain(&mut self, input: usize, gain: f32) -> Result<(), MixError> {
        if input >= MAX_INPUTS || !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
            return Err(MixError::InvalidGain);
        }
        let state = &mut self.gains[input];
        if gain != state.target {
            state.target = gain;
            state.remaining = self.ramp_frames;
            state.step = (gain - state.current) / self.ramp_frames as f32;
        }
        Ok(())
    }
    /// All inputs have equal interleaved stereo lengths. No allocation; empty mix is silence.
    pub fn process(&mut self, inputs: &[&[f32]], output: &mut [f32]) -> Result<(), MixError> {
        if inputs.len() > MAX_INPUTS
            || !output.len().is_multiple_of(2)
            || inputs.iter().any(|x| x.len() != output.len())
        {
            return Err(MixError::InvalidInput);
        }
        let headroom = 1.0 / (inputs.len().max(1) as f32).sqrt();
        for frame in 0..output.len() / 2 {
            let mut mixed = [0.0; 2];
            for (index, input) in inputs.iter().enumerate() {
                let gain = self.gains[index].next();
                for ch in 0..2 {
                    let sample = input[frame * 2 + ch];
                    if sample.is_finite() {
                        mixed[ch] += sample * gain * headroom;
                    }
                }
            }
            if mixed.iter().any(|sample| sample.abs() > 0.8) {
                self.limited_frames = self.limited_frames.saturating_add(1);
            }
            for ch in 0..2 {
                output[frame * 2 + ch] = soft_limit(mixed[ch]);
            }
        }
        Ok(())
    }
    /// Precompute every authorized subset off RT. The callback selects, never sums.
    /// Fixed lanes keep headroom and other inputs' ramps unchanged on joins/leaves.
    pub(crate) fn process_subsets(
        &mut self,
        inputs: &[&[f32]; MAX_INPUTS],
        output: &mut [[[f32; 2]; 1 << MAX_INPUTS]],
    ) -> Result<(), MixError> {
        if inputs.iter().any(|input| input.len() != output.len() * 2) {
            return Err(MixError::InvalidInput);
        }
        let headroom = 1.0 / (MAX_INPUTS as f32).sqrt();
        for (frame, variants) in output.iter_mut().enumerate() {
            let weighted: [[f32; 2]; MAX_INPUTS] = std::array::from_fn(|input| {
                let gain = self.gains[input].next() * headroom;
                std::array::from_fn(|channel| {
                    let sample = inputs[input][frame * 2 + channel];
                    if sample.is_finite() {
                        sample * gain
                    } else {
                        0.0
                    }
                })
            });
            let mut sums = [[0.0; 2]; 1 << MAX_INPUTS];
            for mask in 1usize..1 << MAX_INPUTS {
                let input = mask.trailing_zeros() as usize;
                let previous = mask & !(1 << input);
                if previous == 0 {
                    sums[mask] = weighted[input];
                } else {
                    sums[mask] = std::array::from_fn(|channel| {
                        sums[previous][channel] + weighted[input][channel]
                    });
                }
            }
            variants[0] = [0.0; 2];
            for (variant, sum) in variants[1..].iter_mut().zip(sums.into_iter().skip(1)) {
                *variant = sum.map(soft_limit);
            }
        }
        Ok(())
    }
}
/// Unity below the knee; continuous, monotone and bounded above it.
fn soft_limit(sample: f32) -> f32 {
    let magnitude = sample.abs();
    if magnitude <= 0.8 {
        sample
    } else {
        sample.signum() * (0.8 + 0.2 * (magnitude - 0.8) / (magnitude - 0.6))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subset_mix_limits_aggregate_and_advances_each_gain_once() {
        let mut mixer = Mixer::new(4);
        let mut variants = [[[0.0; 2]; 1 << MAX_INPUTS]; 4];
        let input = [0.7; 8];
        mixer
            .process_subsets(&[&input; MAX_INPUTS], &mut variants)
            .unwrap();
        assert_eq!(mixer.limited_frames(), 0); // Precomputation is not audible limiter activity.
        assert_eq!(variants[0][0], [0.0; 2]);
        assert!((variants[0][1][0] - 0.7 / 3.0f32.sqrt()).abs() < 1e-6);
        assert!(variants[0][7][0] > 0.8 && variants[0][7][0] < 1.0);
        mixer.set_gain(0, 0.0).unwrap();
        mixer
            .process_subsets(&[&input; MAX_INPUTS], &mut variants)
            .unwrap();
        assert_eq!(variants[3][1], [0.0; 2]);
        for frame in variants {
            assert!((frame[2][0] - 0.7 / 3.0f32.sqrt()).abs() < 1e-6);
            assert!((frame[6][0] - soft_limit(1.4 / 3.0f32.sqrt())).abs() < 1e-6);
        }
    }
    #[test]
    fn sum_headroom_limiter_and_ramp() {
        let mut mixer = Mixer::new(4);
        let mut output = [0.0; 8];
        mixer
            .process(&[&[0.1; 8], &[0.1; 8], &[0.1; 8]], &mut output)
            .unwrap();
        assert!((output[0] - 0.3 / 3.0f32.sqrt()).abs() < 1e-6);
        mixer.process(&[&[10.0; 8][..]; 3], &mut output).unwrap();
        assert!(output.iter().all(|x| *x < 1.0 && *x > 0.8));
        assert_eq!(mixer.limited_frames(), 4);
        mixer.set_gain(0, 0.0).unwrap();
        mixer.process(&[&[0.4; 8]], &mut output).unwrap();
        assert!((output[0] - 0.3).abs() < 1e-6);
        assert_eq!(output[6], 0.0);
        mixer.process(&[], &mut output).unwrap();
        assert_eq!(output, [0.0; 8]);
        assert_eq!(mixer.set_gain(0, f32::NAN), Err(MixError::InvalidGain));
    }
}
