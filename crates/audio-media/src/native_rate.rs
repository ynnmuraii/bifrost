//! Fixed nominal-rate conversion on codec workers only, separate from slow clock trim.
use crate::pipeline::MediaError;
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
pub(crate) struct NativeRate {
    inner: Async<f32>,
}
impl NativeRate {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        chunk: usize,
        fixed: FixedAsync,
    ) -> Result<Self, MediaError> {
        let params = SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: None,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = Async::new_sinc(
            f64::from(output_rate) / f64::from(input_rate),
            1.0,
            &params,
            chunk,
            2,
            fixed,
        )
        .map_err(|_| MediaError::Resample)?;
        Ok(Self { inner })
    }
    pub fn input_frames(&self) -> usize {
        self.inner.input_frames_next()
    }
    pub fn max_input_frames(&self) -> usize {
        self.inner.input_frames_max()
    }
    pub fn max_output_frames(&self) -> usize {
        self.inner.output_frames_max()
    }
    /// Group delay in output-rate frames, as reported by the active rubato converter.
    pub fn output_delay(&self) -> usize {
        self.inner.output_delay()
    }
    pub fn reset(&mut self) {
        self.inner.reset();
    }
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<usize, MediaError> {
        let input =
            InterleavedSlice::new(input, 2, input.len() / 2).map_err(|_| MediaError::Resample)?;
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn non_48k_rates_have_bounded_buffers_and_finite_output() {
        for (input, output) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (48_000, 96_000),
        ] {
            let mut converter = NativeRate::new(input, output, 480, FixedAsync::Output).unwrap();
            assert!(converter.inner.output_delay() as f64 / f64::from(output) < 0.01);
            let pcm = vec![0.1; converter.max_input_frames() * 2];
            let mut out = vec![0.0; converter.max_output_frames() * 2];
            for _ in 0..10 {
                let n = converter
                    .process(&pcm[..converter.input_frames() * 2], &mut out)
                    .unwrap();
                assert_eq!(n, 480);
                assert!(out[..n * 2].iter().all(|x| x.is_finite()));
            }
        }
    }
}
