//! KVM-010 spike: local capture on Windows.
//!
//! Prints normalised events (physical HID usages only — never text), measures the
//! event rate so a 1000 Hz mouse is visible, lets you toggle Remote mode with
//! `F11`, and returns to local mode with the emergency combination.
//!
//! ```text
//! cargo run -p platform-win --example spike_capture -- --seconds 5
//! cargo run -p platform-win --example spike_capture -- --seconds 5 --remote --remote-secs 10
//! ```
//!
//! Remote mode suppresses local keyboard and mouse and holds the cursor, so it is
//! strictly opt-in (`--remote`) and always ends on a deadline: a human must be at
//! the keyboard.

#[cfg(not(windows))]
fn main() {
    eprintln!("spike_capture is Windows-only (KVM-010 detects Windows input)");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use input_model::{CaptureKind, HidKey, InputEvent, KeyState};
    use platform_win::capture::{Capture, CaptureConfig, CaptureEvent, CaptureStats};
    use platform_win::dpi;
    use platform_win::event::EmergencyCombo;

    /// `F11` (HID usage `0x44`), the spike's mode-toggle key; the emergency
    /// combination stays `Ctrl+Alt+Shift+F12`.
    const TOGGLE_KEY: HidKey = HidKey::keyboard(0x44);

    /// How many events are printed verbatim before only counters are shown.
    const VERBOSE_EVENTS: u64 = 25;

    struct Args {
        seconds: u64,
        remote: bool,
        remote_secs: u64,
        queue_capacity: usize,
        raw_input: bool,
        capture_injected: bool,
    }

    pub fn run() -> ExitCode {
        let args = match parse_args() {
            Ok(args) => args,
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::from(2);
            }
        };

        println!("== lws spike_capture (KVM-010) ==");
        report_dpi();

        let config = CaptureConfig {
            mode: CaptureKind::Observe,
            emergency: EmergencyCombo::default(),
            queue_capacity: args.queue_capacity,
            raw_input: args.raw_input,
            capture_injected: args.capture_injected,
        };

        let mut capture = match Capture::start(config) {
            Ok(capture) => capture,
            Err(error) => {
                eprintln!("capture failed to start: {error}");
                return ExitCode::FAILURE;
            }
        };

        let stats = capture.stats();
        println!(
            "hooks installed; queue bound {}; raw input {}; emergency Ctrl+Alt+Shift+F12; toggle F11",
            args.queue_capacity,
            if stats.raw_input_ok { "registered" } else { "NOT registered" }
        );
        if args.capture_injected {
            println!(
                "TEST-ONLY: --capture-injected is on, so synthetic input is treated as physical; \
                 an agent run must never use this"
            );
        }
        println!(
            "observe mode for {}s: move the mouse and type, all of it stays local",
            args.seconds
        );

        let outcome = pump(
            &capture,
            Duration::from_secs(args.seconds),
            args.seconds,
            VERBOSE_EVENTS,
        );

        if args.remote && !outcome.emergency {
            run_remote(&capture, &args);
        }

        println!("== summary ==");
        report_stats(&capture.stats());
        println!("final mode: {:?}", capture.mode());
        report_dpi();

        capture.stop();
        ExitCode::SUCCESS
    }

    /// Establishes and reports the DPI awareness before any coordinate is read.
    fn report_dpi() {
        let awareness = dpi::ensure_per_monitor_v2();
        println!(
            "dpi awareness: {awareness} {}",
            if awareness == dpi::DpiAwareness::PerMonitorV2 {
                "[OK: PerMonitorV2, coordinates are physical pixels]"
            } else {
                "[WARN: not PerMonitorV2, coordinates may be virtualised]"
            }
        );
    }

    fn report_stats(stats: &CaptureStats) {
        println!(
            "queued={} dropped={} unmapped_keys={} injected_seen={}",
            stats.captured, stats.dropped, stats.unmapped_keys, stats.injected_seen
        );
        println!(
            "hook_mouse_moves={} raw_mouse_moves={} raw_absolute={} raw_synth_suppressed={}",
            stats.hook_mouse_moves,
            stats.raw_mouse_moves,
            stats.raw_absolute,
            stats.raw_synth_suppressed
        );
        println!(
            "recenters={} emergencies={} queue_overflowed={} raw_input_ok={}",
            stats.recenters, stats.emergencies, stats.overflowed, stats.raw_input_ok
        );
        println!(
            "hook_calls={} (low-level hook callbacks that ran)",
            stats.hook_calls
        );
    }

    struct Outcome {
        emergency: bool,
        verbose: u64,
    }

    /// Dedicated pump: drains the queue, prints at most `verbose` events, prints a
    /// per-second rate line and honours the toggle hotkey and emergency event.
    fn pump(capture: &Capture, total: Duration, seconds: u64, verbose: u64) -> Outcome {
        let start = Instant::now();
        let deadline = start + total;
        let mut last_report = start;
        let mut window = RateWindow::default();
        let mut outcome = Outcome {
            emergency: false,
            verbose: 0,
        };

        let mut counts = StatsSnapshot::default();

        while Instant::now() < deadline {
            match capture.recv_timeout(Duration::from_millis(100)) {
                Ok(CaptureEvent::Input(event)) => {
                    outcome.verbose += 1;
                    if outcome.verbose <= verbose {
                        println!("  {}", describe(&event));
                    }
                    window.note(&event);
                    if let InputEvent::Key {
                        key,
                        state: KeyState::Down,
                    } = event
                    {
                        if key == TOGGLE_KEY {
                            let next = match capture.mode() {
                                CaptureKind::Observe => CaptureKind::Remote,
                                CaptureKind::Temporary | CaptureKind::Remote => {
                                    CaptureKind::Observe
                                }
                            };
                            capture.set_mode(next);
                            println!("F11: mode -> {next:?}");
                        }
                    }
                }
                Ok(CaptureEvent::Emergency) => {
                    println!("EMERGENCY: local mode restored by the hook");
                    outcome.emergency = true;
                    break;
                }
                Ok(CaptureEvent::HotkeyPressed(action)) => {
                    println!("hotkey action {action}");
                }
                Ok(CaptureEvent::QueueOverflow) => {
                    println!("QUEUE OVERFLOW: agent must end the remote session");
                }
                Err(_) => {}
            }

            if last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                let stats = capture.stats();
                let elapsed = start.elapsed().as_secs().max(1);
                println!(
                    "[{elapsed:>3}s/{seconds}s] {} mode={:?} | queued {} dropped {} | {}",
                    window.render(),
                    capture.mode(),
                    stats.captured - counts.captured,
                    stats.dropped - counts.dropped,
                    stats_line(&stats, &mut counts),
                );
            }
        }

        outcome
    }

    fn stats_line(stats: &CaptureStats, previous: &mut StatsSnapshot) -> String {
        let line = format!(
            "hook_moves={} raw_moves={} abs={} supp={} recenters={}",
            stats.hook_mouse_moves - previous.hook_moves,
            stats.raw_mouse_moves - previous.raw_moves,
            stats.raw_absolute - previous.raw_absolute,
            stats.raw_synth_suppressed - previous.raw_synth,
            stats.recenters - previous.recenters,
        );
        previous.captured = stats.captured;
        previous.dropped = stats.dropped;
        previous.hook_moves = stats.hook_mouse_moves;
        previous.raw_moves = stats.raw_mouse_moves;
        previous.raw_absolute = stats.raw_absolute;
        previous.raw_synth = stats.raw_synth_suppressed;
        previous.recenters = stats.recenters;
        line
    }

    fn run_remote(capture: &Capture, args: &Args) {
        println!();
        println!(
            "entering REMOTE in 3s: local keyboard and mouse are suppressed and the cursor is held."
        );
        println!("press Ctrl+Alt+Shift+F12 (always) or F11 (toggle) to return to local.");
        for remaining in (1..=3).rev() {
            println!("  {remaining}...");
            std::thread::sleep(Duration::from_secs(1));
        }

        capture.set_mode(CaptureKind::Remote);
        match capture.hold_point() {
            Some((x, y)) => println!("REMOTE: cursor held at ({x},{y})"),
            None => println!("REMOTE: no hold point (GetCursorPos failed)"),
        }

        let outcome = pump(
            capture,
            Duration::from_secs(args.remote_secs),
            args.remote_secs,
            0,
        );
        capture.set_mode(CaptureKind::Observe);
        println!(
            "left Remote (emergency={}), local input restored",
            outcome.emergency
        );
    }

    fn describe(event: &InputEvent) -> String {
        match *event {
            InputEvent::Key { key, state } => format!(
                "key page=0x{:04X} usage=0x{:04X} {}",
                key.page,
                key.usage,
                if state.is_down() { "down" } else { "up" }
            ),
            InputEvent::Button { button, state } => {
                format!(
                    "button {button:?} {}",
                    if state.is_down() { "down" } else { "up" }
                )
            }
            InputEvent::PointerRelative { dx, dy } => format!("pointer rel dx={dx} dy={dy}"),
            InputEvent::PointerAbsolute(position) => {
                format!("pointer abs x={} y={}", position.x, position.y)
            }
            InputEvent::Scroll { h120, v120 } => format!("wheel h120={h120} v120={v120}"),
        }
    }

    #[derive(Default)]
    struct RateWindow {
        keys: u64,
        buttons: u64,
        rel: u64,
        abs: u64,
        wheel: u64,
    }

    impl RateWindow {
        fn note(&mut self, event: &InputEvent) {
            match event {
                InputEvent::Key { .. } => self.keys += 1,
                InputEvent::Button { .. } => self.buttons += 1,
                InputEvent::PointerRelative { .. } => self.rel += 1,
                InputEvent::PointerAbsolute(_) => self.abs += 1,
                InputEvent::Scroll { .. } => self.wheel += 1,
            }
        }

        fn render(&mut self) -> String {
            let rendered = format!(
                "key={} btn={} rel={} abs={} wheel={}",
                self.keys, self.buttons, self.rel, self.abs, self.wheel
            );
            *self = Self::default();
            rendered
        }
    }

    #[derive(Default)]
    struct StatsSnapshot {
        captured: u64,
        dropped: u64,
        hook_moves: u64,
        raw_moves: u64,
        raw_absolute: u64,
        raw_synth: u64,
        recenters: u64,
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            seconds: 3,
            remote: false,
            remote_secs: 8,
            queue_capacity: 4096,
            raw_input: true,
            capture_injected: false,
        };

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--seconds" => args.seconds = next_u64(&mut iter, "--seconds")?,
                "--remote-secs" => args.remote_secs = next_u64(&mut iter, "--remote-secs")?,
                "--queue" => {
                    args.queue_capacity =
                        next_u64(&mut iter, "--queue")?.clamp(16, 1_000_000) as usize;
                }
                "--remote" => args.remote = true,
                "--no-raw-input" => args.raw_input = false,
                "--capture-injected" => args.capture_injected = true,
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown argument {other}\n{}", usage())),
            }
        }

        Ok(args)
    }

    fn next_u64(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
        iter.next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| format!("{flag} needs a number"))
    }

    fn usage() -> String {
        "usage: spike_capture [--seconds N] [--remote] [--remote-secs N] [--queue N] \
         [--no-raw-input] [--capture-injected]"
            .to_owned()
    }
}
