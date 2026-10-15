//! The glyph particles: a fixed array, a seeded generator, no allocation.
//!
//! Positions are in strip space: `u` along the edge, `v` the depth into the display.

use crate::DEPTH_PX;

/// The hard cap on live glyphs, whatever the event rate: the array never grows.
pub const N: usize = 40;
/// Number of distinct glyphs in the font (`raster::GLYPHS`).
pub const GLYPH_KINDS: u8 = 13;
const PEAK: f32 = 0.85;
/// Glyphs stay this far from the strip's far side so a whole cell fits (logical px).
const V_MAX: f32 = DEPTH_PX - 16.0;

/// xorshift32: deterministic, tiny, good enough for scatter.
#[derive(Debug, Clone)]
pub struct Rng(u32);

impl Rng {
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in `0..1`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Particle {
    pub u: f32,
    pub v: f32,
    home_u: f32,
    home_v: f32,
    vel_u: f32,
    vel_v: f32,
    /// `0..1`, fixed per particle: which glyphs appear first and which stay accented.
    pub rank: f32,
    peak: f32,
    pub alpha: f32,
    pub glyph: u8,
}

#[derive(Debug, Clone)]
pub struct Sim {
    pub particles: [Particle; N],
    rng: Rng,
}

impl Sim {
    #[must_use]
    pub fn new(seed: u32) -> Self {
        Self {
            particles: [Particle::default(); N],
            rng: Rng::new(seed),
        }
    }

    /// Re-seeds every particle's home along a strip of `len` px, invisible. Reuses the array.
    pub fn scatter(&mut self, len: f32) {
        for (index, p) in self.particles.iter_mut().enumerate() {
            p.home_u = self.rng.unit() * len;
            p.home_v = 2.0 + self.rng.unit() * (V_MAX - 2.0);
            p.u = p.home_u;
            p.v = p.home_v;
            p.vel_u = 0.0;
            p.vel_v = 0.0;
            p.rank = (index as f32 + 0.5) / N as f32;
            p.peak = PEAK;
            p.alpha = 0.0;
            p.glyph = (self.rng.next_u32() % u32::from(GLYPH_KINDS)) as u8;
        }
    }

    /// One step of the approach / source fade: homes pulled toward `point` by `pull`
    /// (`0..=1`), visibility by `intensity`, everything scaled by `envelope`.
    pub fn gather(
        &mut self,
        dt: f32,
        len: f32,
        point: f32,
        intensity: f32,
        pull: f32,
        envelope: f32,
    ) {
        let k = 1.0 - (-8.0 * dt).exp();
        for p in &mut self.particles {
            p.home_u = (p.home_u + (self.rng.unit() - 0.5) * 24.0 * dt).clamp(0.0, len);
            let target_u = p.home_u + (point - p.home_u) * pull;
            let target_v = p.home_v * (1.0 - 0.6 * intensity);
            p.u += (target_u - p.u) * k;
            p.v += (target_v - p.v) * k;
            let want = ((intensity * 1.25 - p.rank) * 4.0).clamp(0.0, 1.0) * PEAK * envelope;
            p.alpha += (want - p.alpha) * k;
        }
    }

    /// Starts the release: every particle at the entry `point`, flying away from it.
    pub fn release(&mut self, point: f32) {
        for p in &mut self.particles {
            p.u = point + (self.rng.unit() - 0.5) * 6.0;
            p.v = self.rng.unit() * 4.0;
            p.vel_u = (self.rng.unit() - 0.5) * 320.0;
            p.vel_v = 10.0 + self.rng.unit() * 90.0;
            p.peak = PEAK * (0.5 + 0.5 * self.rng.unit());
            p.alpha = p.peak;
            p.glyph = (self.rng.next_u32() % u32::from(GLYPH_KINDS)) as u8;
        }
    }

    /// One step of the release: drift outward with drag, fade by `envelope`.
    pub fn disperse(&mut self, dt: f32, envelope: f32) {
        let drag = (-2.5 * dt).exp();
        for p in &mut self.particles {
            p.u += p.vel_u * dt;
            p.v += p.vel_v * dt;
            p.vel_u *= drag;
            p.vel_v *= drag;
            p.alpha = p.peak * envelope;
        }
    }

    pub fn hide(&mut self) {
        for p in &mut self.particles {
            p.alpha = 0.0;
        }
    }

    /// Glyphs currently visible.
    #[must_use]
    pub fn visible(&self) -> usize {
        self.particles.iter().filter(|p| p.alpha > 0.03).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_particles() {
        let mut a = Sim::new(7);
        let mut b = Sim::new(7);
        a.scatter(500.0);
        b.scatter(500.0);
        for _ in 0..10 {
            a.gather(0.033, 500.0, 250.0, 0.8, 0.7, 1.0);
            b.gather(0.033, 500.0, 250.0, 0.8, 0.7, 1.0);
        }
        for (x, y) in a.particles.iter().zip(&b.particles) {
            assert_eq!((x.u, x.v, x.alpha, x.glyph), (y.u, y.v, y.alpha, y.glyph));
        }
        let mut c = Sim::new(8);
        c.scatter(500.0);
        assert_ne!(a.particles[0].home_u, c.particles[0].home_u);
    }

    #[test]
    fn a_closer_pointer_draws_particles_closer_to_the_crossing_point() {
        let spread = |intensity: f32| {
            let mut sim = Sim::new(3);
            sim.scatter(800.0);
            for _ in 0..60 {
                sim.gather(
                    0.033,
                    800.0,
                    400.0,
                    intensity,
                    0.85 * intensity.powf(1.5),
                    1.0,
                );
            }
            sim.particles
                .iter()
                .map(|p| (p.u - 400.0).abs())
                .sum::<f32>()
                / N as f32
        };
        assert!(spread(1.0) < spread(0.5));
        assert!(spread(0.5) < spread(0.1));
    }

    #[test]
    fn more_intensity_shows_more_glyphs_and_none_at_zero() {
        let visible = |intensity: f32| {
            let mut sim = Sim::new(3);
            sim.scatter(800.0);
            for _ in 0..60 {
                sim.gather(0.033, 800.0, 400.0, intensity, 0.5, 1.0);
            }
            sim.visible()
        };
        assert_eq!(visible(0.0), 0);
        assert!(visible(0.4) > 0);
        assert!(visible(1.0) > visible(0.4));
        assert!(visible(1.0) <= N);
    }

    #[test]
    fn a_release_moves_every_glyph_away_from_the_entry_point() {
        let mut sim = Sim::new(5);
        sim.scatter(600.0);
        sim.release(300.0);
        let before: f32 = sim
            .particles
            .iter()
            .map(|p| (p.u - 300.0).abs() + p.v)
            .sum();
        for _ in 0..10 {
            sim.disperse(0.033, 1.0);
        }
        let after: f32 = sim
            .particles
            .iter()
            .map(|p| (p.u - 300.0).abs() + p.v)
            .sum();
        assert!(after > before + 100.0);
    }
}
