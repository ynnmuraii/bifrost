//! The state machine of one overlay: idle → approach → fading → idle on the source, and
//! releasing → idle on the receiver.
//!
//! There is exactly one [`Effect`] per overlay, so a burst of events can never stack
//! animations: a new event *steers* the live one instead of starting a second. All it carries
//! is the strip to cover, the crossing point on it and two levels — one of them capped at 1.
//!
//! An approach ends on a message (`LEFT`, `CLEAR`, `CROSSED`), never on a timer: a pointer
//! that rests inside the reach sends no samples at all, and the glow it earned stays lit and
//! steady until the core says where it went. [`LOST_S`] is only the backstop for a core that
//! died mid-approach.

use crate::raster::{self, Canvas};
use crate::{intensity, Strip, Target, Update, FADE_OUT_S, LOST_S, REACH_PX, RELEASE_S};

/// Where the effect is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Phase {
    /// Nothing drawn, nothing to repaint.
    #[default]
    Idle,
    /// The pointer is near a linked edge; `intensity` follows its distance.
    Approach,
    /// Input crossed away (or the core went silent): fading, `t` seconds in.
    Fading { t: f32 },
    /// Input arrived here: the bloom fades out, `t` seconds in.
    Releasing { t: f32 },
}

#[derive(Debug, Clone, Default)]
pub struct Effect {
    phase: Phase,
    strip: Option<Strip>,
    /// The crossing point along the strip (`0..strip.len`, logical px).
    point: f32,
    /// The level the approach is heading for.
    goal: f32,
    /// The level the approach is at: how bright the glow is while it lasts.
    intensity: f32,
    stale: f32,
}

impl Effect {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            strip: None,
            point: 0.0,
            goal: 0.0,
            intensity: 0.0,
            stale: 0.0,
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

    /// How bright the glow is drawn this instant, `0..=1`: the approach level while
    /// approaching or fading, the bloom's own envelope while releasing.
    #[must_use]
    pub fn alpha(&self) -> f32 {
        match self.phase {
            Phase::Idle => 0.0,
            Phase::Approach => self.intensity,
            Phase::Fading { t } => self.intensity * (1.0 - t / FADE_OUT_S).max(0.0),
            Phase::Releasing { t } => {
                let rest = (1.0 - t / RELEASE_S).max(0.0);
                rest * rest
            }
        }
    }

    /// Applies one message from the core. Never allocates.
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Proximity { target, distance } => {
                let resumed = !matches!(self.phase, Phase::Approach);
                self.begin(&target);
                self.goal = intensity(distance, REACH_PX);
                // Steering: coming back out of a fade keeps what was already showing.
                if resumed {
                    self.intensity = self.intensity.max(self.goal);
                }
                self.stale = 0.0;
                self.phase = Phase::Approach;
            }
            Update::Crossed(target) => match self.phase {
                Phase::Approach | Phase::Fading { .. } => self.phase = Phase::Fading { t: 0.0 },
                Phase::Idle | Phase::Releasing { .. } => {
                    if let Some(target) = target {
                        self.begin(&target);
                        self.intensity = 0.7;
                        self.goal = 0.7;
                        self.phase = Phase::Fading { t: 0.0 };
                    }
                }
            },
            Update::Entered(target) => {
                self.begin(&target);
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
    }

    /// Points the effect at `target`, keeping the level it is already showing. Only another
    /// strip (an edge, a display or a segment the effect was not on) starts it over: that is
    /// what makes a burst of events one continuous glow.
    fn begin(&mut self, target: &Target) {
        let strip = Strip::of(target);
        self.point = (target.along - strip.start).clamp(0.0, strip.len);
        // A segment longer than the window carries it along with the pointer, so one sample
        // gives a strip that overlaps this one by a pixel less: the same glow, moving. Only a
        // window that touches nothing of the old one is somewhere else entirely.
        if self.strip.is_some_and(|was| !touches(was, strip)) {
            self.intensity = 0.0;
        }
        self.strip = Some(strip);
    }

    /// Advances by `dt` seconds. Returns whether the effect is still active.
    pub fn step(&mut self, dt: f32) -> bool {
        let Some(_strip) = self.strip else {
            self.phase = Phase::Idle;
            return false;
        };
        let dt = dt.clamp(0.0, 0.1);
        match self.phase {
            Phase::Idle => {}
            Phase::Approach => {
                self.stale += dt;
                if self.stale > LOST_S {
                    self.phase = Phase::Fading { t: 0.0 };
                } else {
                    self.intensity += (self.goal - self.intensity) * (1.0 - (-10.0 * dt).exp());
                }
            }
            Phase::Fading { t } => {
                let t = t + dt;
                if t >= FADE_OUT_S {
                    self.clear();
                } else {
                    self.phase = Phase::Fading { t };
                }
            }
            Phase::Releasing { t } => {
                let t = t + dt;
                if t >= RELEASE_S {
                    self.clear();
                } else {
                    self.phase = Phase::Releasing { t };
                }
            }
        }
        self.is_active()
    }

    /// Rasterizes the current frame into `canvas`, which covers [`Self::strip`] at `scale`
    /// physical px per logical px. Clears the canvas first; allocation-free.
    pub fn draw(&self, canvas: &mut Canvas<'_>, scale: f32) {
        canvas.clear();
        let Some(strip) = self.strip() else {
            return;
        };
        raster::glow(canvas, strip.edge, self.point * scale, self.alpha());
    }
}

/// Whether two strips share any of their edge: the glow can carry on from one to the other,
/// so a new one is not a restart. Two windows over the same segment always do; another
/// display's segment does not.
fn touches(a: Strip, b: Strip) -> bool {
    a.edge == b.edge && a.start < b.start + b.len && b.start < a.start + a.len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Edge, FRAME_S, MAX_ALPHA, MAX_STRIP_PX};
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

    /// One step of a ±`px` tremor: a fixed seed makes the same walk every run, so a failure
    /// is reproducible.
    fn tremor(seed: &mut u32, px: f32) -> f32 {
        (fraction(seed) - 0.5) * 2.0 * px
    }

    /// The next number of a small LCG, in `0..1`. The tests want a repeatable sequence of
    /// gaps and offsets, not a random one, and no dependency on a generator crate.
    fn fraction(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 8) as f32 / 16_777_216.0
    }

    /// The brightest drawn pixel's coordinates and alpha at `scale`.
    fn peak(effect: &Effect, scale: f32) -> (usize, usize, u32) {
        let Some(strip) = effect.strip() else {
            return (0, 0, 0);
        };
        let (w, h) = strip.size_px(scale);
        let mut px = vec![0u32; (w * h) as usize];
        effect.draw(&mut Canvas::new(&mut px, w as usize, h as usize), scale);
        let (index, pixel) = px
            .iter()
            .enumerate()
            .max_by_key(|(_, p)| *p >> 24)
            .expect("a canvas");
        (index % w as usize, index / w as usize, pixel >> 24)
    }

    #[test]
    fn source_goes_idle_approach_fading_idle() {
        let mut fx = Effect::new();
        assert_eq!(fx.phase(), Phase::Idle);
        assert_eq!(fx.alpha(), 0.0);
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 40.0,
        });
        assert_eq!(fx.phase(), Phase::Approach);
        run(&mut fx, 0.2);
        let approaching = peak(&fx, 1.0).2;
        assert!(approaching > 0);
        // The closer the pointer is to the crossing point, the brighter the glow.
        let mut nearer = Effect::new();
        nearer.update(Update::Proximity {
            target: target(500.0),
            distance: 4.0,
        });
        run(&mut nearer, 0.2);
        assert!(peak(&nearer, 1.0).2 > approaching);
        fx.update(Update::Crossed(None));
        assert!(matches!(fx.phase(), Phase::Fading { .. }));
        run(&mut fx, FADE_OUT_S / 2.0);
        assert!(
            peak(&fx, 1.0).2 < approaching,
            "the source fade does not fall"
        );
        run(&mut fx, FADE_OUT_S);
        assert_eq!(fx.phase(), Phase::Idle);
        assert!(fx.strip().is_none());
        assert_eq!(fx.alpha(), 0.0);
    }

    #[test]
    fn receiver_goes_releasing_idle_and_stops_by_itself() {
        let mut fx = Effect::new();
        fx.update(Update::Entered(target(300.0)));
        assert!(matches!(fx.phase(), Phase::Releasing { .. }));
        // A brief soft bloom: full at the entry, gone in RELEASE_S.
        assert_eq!(fx.alpha(), 1.0);
        run(&mut fx, RELEASE_S / 2.0);
        assert!(fx.alpha() < 0.5, "the bloom lingers: {}", fx.alpha());
        run(&mut fx, RELEASE_S);
        assert_eq!(fx.phase(), Phase::Idle);
        assert!(!fx.step(0.033));
    }

    #[test]
    fn an_approach_the_core_goes_silent_about_fades_by_itself() {
        let mut fx = Effect::new();
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 10.0,
        });
        run(&mut fx, LOST_S + FADE_OUT_S + 0.2);
        assert_eq!(fx.phase(), Phase::Idle);
    }

    /// The owner's report: the cursor rests near the edge and trembles. The glow it earned
    /// must hold its level — no fade, no restart — however irregular the samples are.
    #[test]
    fn a_trembling_pointer_inside_the_reach_neither_fades_nor_restarts() {
        let mut fx = Effect::new();
        let mut seed = 1u32;
        // 40 px from the edge, ±2 px of tremor, and along the edge too: a 1080 px segment
        // means every sample slides the window, which used to restart the glow.
        let steady = intensity(40.0, REACH_PX);
        let mut t = 0.0;
        let mut next = 0.0;
        let mut previous = 0.0;
        let mut floor = f32::MAX;
        while t < 5.0 {
            if t >= next {
                fx.update(Update::Proximity {
                    target: target(500.0 + tremor(&mut seed, 2.0)),
                    distance: 40.0 + tremor(&mut seed, 2.0),
                });
                // The OS hands the core samples 30..300 ms apart.
                next = t + 0.030 + 0.270 * fraction(&mut seed);
            }
            assert_eq!(
                fx.phase(),
                Phase::Approach,
                "the glow left the approach at {t}"
            );
            fx.step(FRAME_S);
            let alpha = fx.alpha();
            if t > 0.5 {
                assert!(
                    alpha >= previous * 0.9,
                    "the glow flickered at {t}: {previous} → {alpha}"
                );
                floor = floor.min(alpha);
            }
            previous = alpha;
            t += FRAME_S;
        }
        assert!(
            floor >= steady * 0.9,
            "the glow dimmed to {floor}, under the {steady} it should hold"
        );
    }

    /// A pointer that stops inside the reach: the core sends nothing more, and the glow stays
    /// up and steady instead of fading out under it.
    #[test]
    fn a_resting_pointer_inside_the_reach_keeps_the_glow_up() {
        let mut fx = Effect::new();
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 40.0,
        });
        run(&mut fx, 3.0);
        assert_eq!(
            fx.phase(),
            Phase::Approach,
            "the glow faded while the pointer stayed"
        );
        assert!(fx.is_active());
        assert!(fx.alpha() > 0.8, "the glow dimmed: {}", fx.alpha());
    }

    /// Leaving the reach fades the glow out over the existing fade time — no lingering, and
    /// no cutting it short.
    #[test]
    fn leaving_the_reach_fades_out_within_the_fade_time() {
        let mut fx = Effect::new();
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 0.0,
        });
        run(&mut fx, 0.3);
        fx.update(Update::Left);
        assert!(matches!(fx.phase(), Phase::Fading { .. }));
        run(&mut fx, FADE_OUT_S - 0.05);
        assert!(fx.is_active(), "the fade ended early");
        assert!(fx.alpha() < 0.2, "still bright: {}", fx.alpha());
        run(&mut fx, 0.15);
        assert_eq!(fx.phase(), Phase::Idle);
    }

    /// A walk along a long segment: the window slides a pixel with the pointer, and the glow
    /// keeps the level it had — the same effect, moving, not a new one every sample.
    #[test]
    fn walking_along_the_edge_keeps_the_level() {
        let mut fx = Effect::new();
        let steady = intensity(40.0, REACH_PX);
        let mut floor = f32::MAX;
        for step in 0..90 {
            fx.update(Update::Proximity {
                target: target(500.0 + step as f32),
                distance: 40.0,
            });
            fx.step(FRAME_S);
            if step >= 15 {
                floor = floor.min(fx.alpha());
            }
        }
        // Without the window fix every sample zeroes the level: the drawn alpha sits at the
        // one-frame share of the smoothing, ~0.28 of the steady value.
        assert!(
            floor >= steady * 0.9,
            "the glow restarted on every step: {floor} against {steady}"
        );
    }

    /// The core's hysteresis keeps a pointer on the boundary inside the band, so the overlay
    /// only ever sees approaches: nothing to fade and nothing to restart.
    #[test]
    fn a_pointer_oscillating_on_the_reach_boundary_only_steers() {
        let mut fx = Effect::new();
        for step in 0..90 {
            let distance = if step % 2 == 0 {
                REACH_PX - 5.0
            } else {
                REACH_PX + 5.0
            };
            fx.update(Update::Proximity {
                target: target(500.0),
                distance,
            });
            fx.step(FRAME_S);
            assert_eq!(
                fx.phase(),
                Phase::Approach,
                "the boundary flipped the effect"
            );
        }
    }

    #[test]
    fn leaving_the_reach_fades_and_clear_stops_at_once() {
        let mut fx = Effect::new();
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 10.0,
        });
        fx.update(Update::Left);
        assert!(matches!(fx.phase(), Phase::Fading { .. }));
        fx.update(Update::Clear);
        assert_eq!(fx.phase(), Phase::Idle);
    }

    #[test]
    fn a_fading_effect_that_the_pointer_comes_back_to_steers_without_a_restart() {
        let mut fx = Effect::new();
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 5.0,
        });
        run(&mut fx, 0.3);
        let bright = fx.alpha();
        fx.update(Update::Crossed(None));
        run(&mut fx, 0.1);
        assert!(fx.alpha() < bright);
        fx.update(Update::Proximity {
            target: target(500.0),
            distance: 100.0,
        });
        assert_eq!(fx.phase(), Phase::Approach);
        // Same strip: the glow picked up where it was rather than dropping to nothing.
        assert!(
            fx.alpha() >= bright * 0.5,
            "the glow blinked: {}",
            fx.alpha()
        );
        assert!(fx.alpha() > intensity(100.0, REACH_PX));
    }

    /// A glow at `along` lights the part of the strip under it, not the middle of the strip.
    #[test]
    fn the_glow_is_brightest_where_the_cursor_will_cross() {
        for along in [200.0, 500.0, 900.0] {
            let mut fx = Effect::new();
            // The core samples the pointer at ~30 Hz while the effect is alive.
            for _ in 0..30 {
                fx.update(Update::Proximity {
                    target: target(along),
                    distance: 0.0,
                });
                fx.step(0.033);
            }
            let strip = fx.strip().expect("an active effect");
            let (w, h) = strip.size_px(1.0);
            let mut px = vec![0u32; (w * h) as usize];
            fx.draw(&mut Canvas::new(&mut px, w as usize, h as usize), 1.0);
            let peak = px.iter().map(|p| p >> 24).max().expect("a canvas");
            assert!(peak > 90, "too faint: {peak}");
            // A right edge: `x` is the depth (the edge itself is the last column) and `y` is
            // the along axis, where the crossing point sits `along - strip.start` px into it.
            let y = (along - strip.start) as usize;
            assert_eq!(
                px[y * w as usize + w as usize - 1] >> 24,
                peak,
                "the glow is not on the edge at {along}"
            );
            assert_eq!(
                px[y * w as usize] >> 24,
                0,
                "lit at the far side at {along}"
            );
        }
    }

    /// Twenty crossings a second back and forth play out as one effect: one live instance, a
    /// strip that never grows past the glow, the frames allocate nothing, and it ends right
    /// after the last event.
    #[test]
    fn a_ping_pong_burst_plays_as_one_effect_without_allocating() {
        let mut fx = Effect::new();
        // Warm-up: the first events may touch anything.
        fx.update(Update::Entered(target(500.0)));
        fx.step(0.033);
        let (w, h) = Strip::of(&target(500.0)).size_px(2.0);
        let mut canvas_buf = vec![0u32; (w * h) as usize];
        let before = ALLOCS.with(Cell::get);
        let mut peak_alpha = 0;
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
            let mut canvas = Canvas::new(&mut canvas_buf, w as usize, h as usize);
            fx.draw(&mut canvas, 2.0);
            peak_alpha = peak_alpha.max(canvas_buf.iter().map(|p| p >> 24).max().unwrap_or(0));
            elapsed += 0.033;
        }
        assert!(peak_alpha <= (MAX_ALPHA * 255.0) as u32, "over the cap");
        assert!(fx.strip().is_some_and(|strip| strip.len <= MAX_STRIP_PX));
        // After the last event the effect ends within the bloom time.
        let mut after_last = elapsed - last_event;
        while fx.is_active() {
            fx.step(0.033);
            after_last += 0.033;
            assert!(after_last < RELEASE_S + 0.12, "still running {after_last}");
        }
        assert_eq!(ALLOCS.with(Cell::get) - before, 0, "a frame allocated");
    }
}
