//! The state machine of one overlay: idle → approach → fading → idle on the source, and
//! releasing → idle on the receiver.
//!
//! There is exactly one [`Effect`] per overlay and one fixed particle array inside it, so a
//! burst of events can never stack animations: a new event *restarts or steers* the live
//! one, reusing its particles (the glyph cap is [`crate::sim::N`] however many arrive).

use crate::raster::{Canvas, ACCENT, DIM};
use crate::sim::Sim;
use crate::{intensity, Edge, Strip, Target, Update, FADE_OUT_S, REACH_PX, RELEASE_S, STALE_S};

/// Where the effect is in its life.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Nothing drawn, nothing to repaint.
    Idle,
    /// The pointer is near a linked edge; `intensity` follows its distance.
    Approach,
    /// Input crossed away (or the approach went stale): collapsing and fading, `t` seconds in.
    Fading { t: f32 },
    /// Input arrived here: the glyphs disperse, `t` seconds in.
    Releasing { t: f32 },
}

#[derive(Debug, Clone)]
pub struct Effect {
    phase: Phase,
    strip: Option<Strip>,
    /// The crossing point along the strip (`0..len`).
    point: f32,
    goal: f32,
    intensity: f32,
    stale: f32,
    sim: Sim,
}

impl Effect {
    #[must_use]
    pub fn new(seed: u32) -> Self {
        Self {
            phase: Phase::Idle,
            strip: None,
            point: 0.0,
            goal: 0.0,
            intensity: 0.0,
            stale: 0.0,
            sim: Sim::new(seed),
        }
    }

    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the platform loop must keep stepping and repainting.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// The strip to cover while active.
    #[must_use]
    pub fn strip(&self) -> Option<Strip> {
        if self.is_active() {
            self.strip
        } else {
            None
        }
    }

    #[must_use]
    pub fn visible_glyphs(&self) -> usize {
        self.sim.visible()
    }

    /// Applies one message from the core. Never allocates.
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Proximity { target, distance } => {
                let goal = intensity(distance, REACH_PX);
                let resumed = !matches!(self.phase, Phase::Approach);
                self.begin(&target, resumed);
                // Steering: coming back out of a fade keeps what was already showing.
                self.goal = goal;
                if resumed {
                    self.intensity = self.intensity.max(goal);
                }
                self.stale = 0.0;
                self.phase = Phase::Approach;
            }
            Update::Crossed(target) => match self.phase {
                Phase::Approach | Phase::Fading { .. } => self.phase = Phase::Fading { t: 0.0 },
                Phase::Idle | Phase::Releasing { .. } => {
                    if let Some(target) = target {
                        self.begin(&target, true);
                        self.intensity = 0.7;
                        self.goal = 0.7;
                        self.phase = Phase::Fading { t: 0.0 };
                    }
                }
            },
            Update::Entered(target) => {
                self.begin(&target, true);
                self.sim.release(self.point);
                self.phase = Phase::Releasing { t: 0.0 };
            }
            Update::Left => {
                if self.phase == Phase::Approach {
                    self.phase = Phase::Fading { t: 0.0 };
                }
            }
            Update::Clear => self.clear(),
        }
    }

    /// Hard stop: idle at once.
    pub fn clear(&mut self) {
        self.phase = Phase::Idle;
        self.strip = None;
        self.intensity = 0.0;
        self.goal = 0.0;
        self.sim.hide();
    }

    /// Points the effect at `target`; re-scatters the particles only when the strip is new
    /// or the effect was not already running on it (`fresh`).
    fn begin(&mut self, target: &Target, fresh: bool) {
        let strip = Strip::of(target);
        let moved = self.strip != Some(strip);
        self.point = (target.along - strip.start).clamp(0.0, strip.len);
        if moved || (fresh && self.phase == Phase::Idle) {
            self.strip = Some(strip);
            self.sim.scatter(strip.len);
            self.intensity = 0.0;
        }
    }

    /// Advances by `dt` seconds. Returns whether the effect is still active.
    pub fn step(&mut self, dt: f32) -> bool {
        let Some(strip) = self.strip else {
            self.phase = Phase::Idle;
            return false;
        };
        let dt = dt.clamp(0.0, 0.1);
        match self.phase {
            Phase::Idle => {}
            Phase::Approach => {
                self.stale += dt;
                if self.stale > STALE_S {
                    self.phase = Phase::Fading { t: 0.0 };
                } else {
                    self.intensity += (self.goal - self.intensity) * (1.0 - (-10.0 * dt).exp());
                    let pull = 0.85 * self.intensity.powf(1.5);
                    self.sim.gather(dt, strip.len, self.point, self.intensity, pull, 1.0);
                }
            }
            Phase::Fading { t } => {
                let t = t + dt;
                if t >= FADE_OUT_S {
                    self.clear();
                } else {
                    self.phase = Phase::Fading { t };
                    let envelope = 1.0 - t / FADE_OUT_S;
                    self.sim
                        .gather(dt, strip.len, self.point, self.intensity, 1.0, envelope);
                }
            }
            Phase::Releasing { t } => {
                let t = t + dt;
                if t >= RELEASE_S {
                    self.clear();
                } else {
                    self.phase = Phase::Releasing { t };
                    let rest = 1.0 - t / RELEASE_S;
                    self.sim.disperse(dt, rest * rest);
                }
            }
        }
        self.is_active()
    }

    /// Rasterizes the current frame into `canvas`, which covers [`Self::strip`] at `scale`
    /// physical px per logical px. Clears the canvas first; allocation-free.
    pub fn draw(&self, canvas: &mut Canvas<'_>, scale: f32) {
        canvas.clear();
        let Some(strip) = self.strip().filter(|_| self.is_active()) else {
            return;
        };
        let cell = (scale * 2.0).round().max(1.0) as i32;
        let (gw, gh) = (5 * cell, 7 * cell);
        let (w, h) = (canvas.width() as f32, canvas.height() as f32);
        for p in &self.sim.particles {
            if p.alpha <= 0.03 {
                continue;
            }
            let (along, depth) = (p.u * scale, p.v * scale);
            let (x, y) = match strip.edge {
                Edge::Top => (along, depth),
                Edge::Bottom => (along, h - depth - gh as f32),
                Edge::Left => (depth, along),
                Edge::Right => (w - depth - gw as f32, along),
            };
            let colour = if p.rank < 0.3 { ACCENT } else { DIM };
            canvas.glyph(
                p.glyph,
                x as i32 - gw / 2 * i32::from(strip.edge.is_horizontal()),
                y as i32 - gh / 2 * i32::from(!strip.edge.is_horizontal()),
                cell,
                colour,
                p.alpha,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static ALLOCS: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;
    // SAFETY: forwards to the system allocator; only counts per thread.
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            let _ = ALLOCS.try_with(|count| count.set(count.get() + 1));
            // SAFETY: same contract as the caller's.
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: same contract as the caller's.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }
    #[global_allocator]
    static GLOBAL: Counting = Counting;

    fn target(along: f32) -> Target {
        Target {
            edge: Edge::Right,
            start: 0.0,
            end: 1080.0,
            along,
        }
    }

    fn run(effect: &mut Effect, seconds: f32) {
        let mut t = 0.0;
        while t < seconds {
            effect.step(0.033);
            t += 0.033;
        }
    }

    #[test]
    fn source_goes_idle_approach_fading_idle() {
        let mut fx = Effect::new(1);
        assert_eq!(fx.phase(), Phase::Idle);
        fx.update(Update::Proximity { target: target(500.0), distance: 40.0 });
        assert_eq!(fx.phase(), Phase::Approach);
        run(&mut fx, 0.2);
        assert!(fx.visible_glyphs() > 0);
        fx.update(Update::Crossed(None));
        assert!(matches!(fx.phase(), Phase::Fading { .. }));
        run(&mut fx, FADE_OUT_S + 0.1);
        assert_eq!(fx.phase(), Phase::Idle);
        assert!(fx.strip().is_none());
        assert_eq!(fx.visible_glyphs(), 0);
    }

    #[test]
    fn receiver_goes_releasing_idle_and_stops_by_itself() {
        let mut fx = Effect::new(1);
        fx.update(Update::Entered(target(300.0)));
        assert!(matches!(fx.phase(), Phase::Releasing { .. }));
        assert!(fx.visible_glyphs() > 0);
        run(&mut fx, RELEASE_S + 0.1);
        assert_eq!(fx.phase(), Phase::Idle);
        assert!(!fx.step(0.033));
    }

    #[test]
    fn an_approach_with_no_updates_fades_by_itself() {
        let mut fx = Effect::new(1);
        fx.update(Update::Proximity { target: target(500.0), distance: 10.0 });
        run(&mut fx, STALE_S + FADE_OUT_S + 0.2);
        assert_eq!(fx.phase(), Phase::Idle);
    }

    #[test]
    fn leaving_the_reach_fades_and_clear_stops_at_once() {
        let mut fx = Effect::new(1);
        fx.update(Update::Proximity { target: target(500.0), distance: 10.0 });
        fx.update(Update::Left);
        assert!(matches!(fx.phase(), Phase::Fading { .. }));
        fx.update(Update::Clear);
        assert_eq!(fx.phase(), Phase::Idle);
    }

    #[test]
    fn a_fading_effect_that_the_pointer_comes_back_to_steers_without_a_restart() {
        let mut fx = Effect::new(1);
        fx.update(Update::Proximity { target: target(500.0), distance: 5.0 });
        run(&mut fx, 0.3);
        fx.update(Update::Crossed(None));
        run(&mut fx, 0.1);
        let before = fx.sim.particles[3].u;
        fx.update(Update::Proximity { target: target(500.0), distance: 100.0 });
        assert_eq!(fx.phase(), Phase::Approach);
        // Same strip: the particles were reused, not re-scattered.
        assert!((fx.sim.particles[3].u - before).abs() < 1e-3);
        assert!(fx.intensity >= intensity(100.0, REACH_PX));
    }

    /// Ten crossings a second back and forth play out as one effect: one live instance, the
    /// glyph cap holds, the frames allocate nothing, and it ends right after the last event.
    #[test]
    fn a_ping_pong_burst_plays_as_one_effect_without_allocating() {
        let mut fx = Effect::new(9);
        // Warm-up: the first events may touch anything.
        fx.update(Update::Entered(target(500.0)));
        fx.step(0.033);
        let mut canvas_buf = vec![0u32; 72 * 1080];
        let before = ALLOCS.with(Cell::get);
        let mut peak_glyphs = 0;
        let mut elapsed = 0.0;
        let mut last_event = 0.0;
        for step in 0..15 {
            // 20 events in ~500 ms: a Crossed and an Entered per ~50 ms.
            for event in 0..2 {
                last_event = elapsed;
                if (step + event) % 2 == 0 {
                    fx.update(Update::Crossed(Some(target(500.0 + step as f32))));
                } else {
                    fx.update(Update::Entered(target(500.0 + step as f32)));
                }
            }
            fx.step(0.033);
            let mut canvas = Canvas::new(&mut canvas_buf, 72, 1080);
            fx.draw(&mut canvas, 2.0 / 2.0);
            peak_glyphs = peak_glyphs.max(fx.visible_glyphs());
            elapsed += 0.033;
        }
        assert!(peak_glyphs <= crate::sim::N);
        // After the last event the effect ends within the release time.
        let mut after_last = elapsed - last_event;
        while fx.is_active() {
            fx.step(0.033);
            after_last += 0.033;
            assert!(after_last < RELEASE_S + 0.12, "still running {after_last}");
        }
        assert_eq!(ALLOCS.with(Cell::get) - before, 0, "a frame allocated");
    }
}
