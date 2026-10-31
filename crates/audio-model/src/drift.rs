//! Slow device-clock PI control, independent of fast packet-jitter adaptation.
//! Ratio is output frames / input frames: positive fill error MUST lower it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoarseCorrection {
    None,
    DropFrame,
    DuplicateFrame,
}
#[derive(Debug, Clone, Copy)]
pub struct DriftUpdate {
    pub ratio: f64,
    pub correction: CoarseCorrection,
}

pub struct DriftController {
    target: f64,
    frame: f64,
    filtered: f64,
    integral: f64,
    ppm: f64,
    last: Option<u64>,
    saturated_since: Option<u64>,
}
impl DriftController {
    /// `target` and `frame_samples` are per-channel PCM frame counts at 48 kHz.
    pub fn new(target: u32, frame_samples: u16) -> Self {
        assert!(target > 0 && matches!(frame_samples, 480 | 960));
        Self {
            target: f64::from(target),
            frame: f64::from(frame_samples),
            filtered: 0.0,
            integral: 0.0,
            ppm: 0.0,
            last: None,
            saturated_since: None,
        }
    }
    /// Reset whenever the device or adaptive jitter target changes.
    pub fn reset(&mut self, target: u32) {
        assert!(target > 0);
        self.target = f64::from(target);
        self.filtered = 0.0;
        self.integral = 0.0;
        self.ppm = 0.0;
        self.last = None;
        self.saturated_since = None;
    }
    /// Current PI actuator setting, before integer telemetry rounding.
    pub fn ratio_ppm(&self) -> f64 {
        self.ppm
    }
    /// Low-pass filtered decoded-ring fill error, in per-channel PCM frames.
    pub fn filtered_fill_error_frames(&self) -> f64 {
        self.filtered
    }
    /// `now_ms` comes from the render clock. Calls faster than 100 ms are ignored.
    /// Normal clamp is ±1000 ppm; slew is at most 100 ppm/s, filter time constant 10 s.
    pub fn update(&mut self, now_ms: u64, fill_samples: f64) -> DriftUpdate {
        let current = DriftUpdate {
            ratio: 1.0 + self.ppm * 1e-6,
            correction: CoarseCorrection::None,
        };
        if !fill_samples.is_finite() {
            return current;
        }
        let Some(last) = self.last else {
            self.last = Some(now_ms);
            return current;
        };
        if now_ms < last || now_ms - last < 100 {
            return current;
        }
        let dt = ((now_ms - last) as f64 / 1000.0).min(1.0);
        self.last = Some(now_ms);
        let error = fill_samples - self.target;
        if error.abs() > self.frame * 2.0 {
            let correction = if error > 0.0 {
                CoarseCorrection::DropFrame
            } else {
                CoarseCorrection::DuplicateFrame
            };
            self.reset(self.target as u32);
            self.last = Some(now_ms);
            return DriftUpdate {
                ratio: 1.0,
                correction,
            };
        }
        self.filtered += dt / (10.0 + dt) * (error - self.filtered);
        // Gains in ppm per sample and ppm per sample-second. Slow enough to reject bursts.
        let candidate = self.integral + self.filtered * dt * 0.08;
        let requested = -self.filtered * 2.0 - candidate;
        // Conditional integration: do not accumulate error into a saturated actuator.
        if requested.abs() <= 1000.0 || requested.signum() != self.ppm.signum() {
            self.integral = candidate;
        }
        let desired = (-self.filtered * 2.0 - self.integral).clamp(-1000.0, 1000.0);
        self.ppm += (desired - self.ppm).clamp(-100.0 * dt, 100.0 * dt);
        if desired.abs() >= 1000.0 {
            let since = *self.saturated_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= 2000 {
                let correction = if error > 0.0 {
                    CoarseCorrection::DropFrame
                } else {
                    CoarseCorrection::DuplicateFrame
                };
                self.reset(self.target as u32);
                self.last = Some(now_ms);
                return DriftUpdate {
                    ratio: 1.0,
                    correction,
                };
            }
        } else {
            self.saturated_since = None;
        }
        DriftUpdate {
            ratio: 1.0 + self.ppm * 1e-6,
            correction: CoarseCorrection::None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sign_and_hard_bounds() {
        let mut c = DriftController::new(1440, 480);
        c.update(0, 1440.0);
        assert!(c.update(100, 1500.0).ratio < 1.0);
        assert!(c.ratio_ppm() < 0.0);
        assert!((c.filtered_fill_error_frames() - 60.0 * 0.1 / 10.1).abs() < 1e-12);
        assert_eq!(
            c.update(200, 2500.0).correction,
            CoarseCorrection::DropFrame
        );
        assert_eq!(
            c.update(300, 300.0).correction,
            CoarseCorrection::DuplicateFrame
        );
        assert_eq!(c.ratio_ppm(), 0.0);
        assert_eq!(c.filtered_fill_error_frames(), 0.0);
        c.update(400, 1380.0);
        assert!(c.ratio_ppm() > 0.0);
        assert!(c.filtered_fill_error_frames() < 0.0);
    }
    #[test]
    fn hundred_ppm_clocks_converge_over_four_hours() {
        for ppm in [-100.0, 100.0] {
            let mut c = DriftController::new(1440, 480);
            let mut fill = 1440.0;
            let mut min = f64::MAX;
            let mut max = f64::MIN;
            for step in 0..144_000_u64 {
                let update = c.update(step * 100, fill);
                assert_eq!(update.correction, CoarseCorrection::None);
                fill += 4800.0 * ((1.0 + ppm * 1e-6) - 1.0 / update.ratio);
                assert!((1300.0..1580.0).contains(&fill), "{ppm}: {fill}");
                if step > 100_000 {
                    min = min.min(fill);
                    max = max.max(fill);
                }
            }
            assert!((fill - 1440.0).abs() < 0.1, "{ppm}: {fill}");
            assert!(max - min < 0.1, "sustained oscillation {min}..{max}");
        }
    }
}
