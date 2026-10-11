//! XTest injection spike (KVM-012).
//!
//! Proves that keys, buttons, wheel and pointer position can be injected through
//! XTest, and that the pressed-state ledger is released on exit.
//!
//! ```text
//! cargo run -p platform-x11 --example spike_x11_inject -- --seconds 10            # observe (moves the pointer in a small square)
//! cargo run -p platform-x11 --example spike_x11_inject -- --seconds 10 --live     # also types "lws", left-clicks and scrolls
//! ```
//!
//! `--observe` is the default and never clicks or types. `--live` does, so point
//! the display under test at something harmless first. Both modes always call
//! `release_all` on the way out, including on a panic.

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use input_model::{HidKey, InputEvent, KeyState, MouseButton, Position};
#[cfg(target_os = "linux")]
use platform_x11::{InjectResult, X11Injector};

#[cfg(target_os = "linux")]
const USAGE: &str =
    "usage: spike_x11_inject [--seconds N] [--observe|--live] (default: --seconds 10 --observe)";

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Observe,
    Live,
}

#[cfg(target_os = "linux")]
struct Options {
    seconds: u64,
    mode: Mode,
}

#[cfg(target_os = "linux")]
impl Options {
    fn from_args(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            seconds: 10,
            mode: Mode::Observe,
        };
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seconds" => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("--seconds needs a value ({USAGE})"))?;
                    options.seconds = value
                        .parse()
                        .map_err(|_| format!("--seconds expects a whole number, got {value:?}"))?;
                }
                "--observe" => options.mode = Mode::Observe,
                "--live" => options.mode = Mode::Live,
                "--help" | "-h" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown argument {other:?} ({USAGE})")),
            }
        }
        Ok(options)
    }
}

/// Owns the injector and guarantees `release_all` runs, even while unwinding.
///
/// X11 key state is server-global, not per-client: a key this process pushed down
/// stays down after the process dies unless it is released first. That is why the
/// release is tied to a `Drop`, not to the happy path.
#[cfg(target_os = "linux")]
struct ReleaseGuard {
    injector: X11Injector,
    released: bool,
}

#[cfg(target_os = "linux")]
impl ReleaseGuard {
    fn new(injector: X11Injector) -> Self {
        Self {
            injector,
            released: false,
        }
    }

    fn injector(&mut self) -> &mut X11Injector {
        &mut self.injector
    }

    /// Releases everything held, once. Reports how many events that took.
    fn release_now(&mut self) -> InjectResult {
        self.released = true;
        self.injector.release_all()
    }
}

#[cfg(target_os = "linux")]
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let result = self.injector.release_all();
        eprintln!(
            "release_all on unwind: {} event(s), error={:?}, ledger now empty={}",
            result.applied_count,
            result.os_error,
            self.injector.is_empty()
        );
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let options = match Options::from_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let injector = match X11Injector::new() {
        Ok(injector) => injector,
        Err(err) => {
            eprintln!("cannot start the XTest injector: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (major, minor) = injector.xtest_version();
    let (width, height) = injector.root_size();
    println!("XTest extension present: version {major}.{minor}");
    println!("root window 0x{:08x}, {width}x{height} px", injector.root());
    match injector.pointer_position() {
        Ok(position) => println!(
            "start pointer position: ({:.0}, {:.0})",
            position.x, position.y
        ),
        Err(err) => println!("start pointer position: unavailable ({err})"),
    }
    println!(
        "start ledger: keys={:?} buttons={:?}",
        injector.pressed_keys(),
        injector.pressed_buttons()
    );

    let mut guard = ReleaseGuard::new(injector);
    let start = guard
        .injector()
        .pointer_position()
        .unwrap_or_else(|_| Position::new(f64::from(width) / 2.0, f64::from(height) / 2.0));
    let deadline = Instant::now() + Duration::from_secs(options.seconds);

    match options.mode {
        Mode::Observe => {
            println!(
                "mode: observe (pointer only) for {} seconds",
                options.seconds
            );
            run_observe(guard.injector(), start, deadline);
        }
        Mode::Live => {
            println!(
                "mode: live for {} seconds -- WARNING: this types, clicks and scrolls on the display under test",
                options.seconds
            );
            run_live(guard.injector(), deadline);
        }
    }

    let released = guard.release_now();
    println!(
        "release_all: {} event(s), error={:?}",
        released.applied_count, released.os_error
    );
    println!(
        "final ledger: keys={:?} buttons={:?} (empty={})",
        guard.injector().pressed_keys(),
        guard.injector().pressed_buttons(),
        guard.injector().is_empty()
    );
    match guard.injector().pointer_position() {
        Ok(position) => println!(
            "final pointer position: ({:.0}, {:.0})",
            position.x, position.y
        ),
        Err(err) => println!("final pointer position: unavailable ({err})"),
    }
    ExitCode::SUCCESS
}

/// Walks the pointer around a small square so the injected motion is visible.
#[cfg(target_os = "linux")]
fn run_observe(injector: &mut X11Injector, start: Position, deadline: Instant) {
    const SIDE: f64 = 60.0;
    let offsets = [(SIDE, SIDE), (SIDE, -SIDE), (-SIDE, -SIDE), (-SIDE, SIDE)];
    let mut step = 0usize;
    while Instant::now() < deadline {
        let (dx, dy) = offsets[step % offsets.len()];
        let target = Position::new(start.x + dx, start.y + dy);
        let result = injector.inject(&[InputEvent::PointerAbsolute(target)]);
        println!(
            "[{step}] observe target=({:.0}, {:.0}) applied={} error={:?} actual={:?}",
            target.x, target.y, result.applied_count, result.os_error, result.actual
        );
        step += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Types `lws`, left-clicks once and scrolls once, then idles so the ledger stays
/// observable for the rest of the window.
#[cfg(target_os = "linux")]
fn run_live(injector: &mut X11Injector, deadline: Instant) {
    for usage in [0x0F_u16, 0x1A, 0x16] {
        let key = HidKey::keyboard(usage);
        let press = injector.inject(&[InputEvent::Key {
            key,
            state: KeyState::Down,
        }]);
        let release = injector.inject(&[InputEvent::Key {
            key,
            state: KeyState::Up,
        }]);
        println!(
            "type 0x{usage:02X}: press applied={} release applied={} error={:?}",
            press.applied_count, release.applied_count, release.os_error
        );
    }

    let click = injector.inject(&[
        InputEvent::Button {
            button: MouseButton::Left,
            state: KeyState::Down,
        },
        InputEvent::Button {
            button: MouseButton::Left,
            state: KeyState::Up,
        },
    ]);
    println!(
        "left click: applied={} error={:?}",
        click.applied_count, click.os_error
    );

    for (label, h120, v120) in [
        ("wheel up", 0, 120),
        ("wheel down", 0, -120),
        ("wheel right", 120, 0),
    ] {
        let result = injector.inject(&[InputEvent::Scroll { h120, v120 }]);
        println!(
            "{label}: applied={} error={:?}",
            result.applied_count, result.os_error
        );
    }

    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "live idle: ledger keys={:?} buttons={:?}",
            injector.pressed_keys(),
            injector.pressed_buttons()
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("platform-x11 spike: Linux-only; run this on an X11 session.");
}
