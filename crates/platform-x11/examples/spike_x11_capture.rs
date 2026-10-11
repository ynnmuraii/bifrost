//! XInput2 raw capture spike (KVM-013).
//!
//! ```text
//! cargo run -p platform-x11 --example spike_x11_capture -- --seconds 10           # observe only, prints raw events
//! cargo run -p platform-x11 --example spike_x11_capture -- --seconds 10 --grab    # takes the pointer/keyboard grab (suppresses local input)
//! ```
//!
//! Without `--grab` this is **observe-only**: raw events are printed but local
//! input is not suppressed and the pointer is not recentered. With `--grab` the
//! pointer and keyboard are grabbed with `owner_events = false`, which is what
//! actually stops local input from reaching the local session.
//!
//! The spike always ungrabs on the way out (`Drop`), and prints the manual checks
//! that cannot be automated: does the emergency combo fire, and does it avoid
//! switching virtual terminals?

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use input_model::InputEvent;
#[cfg(target_os = "linux")]
use platform_x11::capture::RECENTER_THRESHOLD_PX;
#[cfg(target_os = "linux")]
use platform_x11::keymap::evdev_to_x11_keycode;
#[cfg(target_os = "linux")]
use platform_x11::{CapturedEvent, X11Capture};

#[cfg(target_os = "linux")]
const USAGE: &str =
    "usage: spike_x11_capture [--seconds N] [--grab] (default: --seconds 10, observe-only)";

#[cfg(target_os = "linux")]
struct Options {
    seconds: u64,
    grab: bool,
}

#[cfg(target_os = "linux")]
impl Options {
    fn from_args(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            seconds: 10,
            grab: false,
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
                "--grab" => options.grab = true,
                "--help" | "-h" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown argument {other:?} ({USAGE})")),
            }
        }
        Ok(options)
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

    let mut capture = match X11Capture::new(!options.grab) {
        Ok(capture) => capture,
        Err(err) => {
            eprintln!("cannot start XI2 capture: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (major, minor) = capture.xi2_version();
    let (width, height) = capture.root_size();
    println!("XI2 version: {major}.{minor}");
    println!("root window 0x{:08x}, {width}x{height} px", capture.root());
    println!(
        "XTEST devices excluded from capture (injected-loop filter): {:?}",
        capture.xtest_device_ids()
    );
    if options.grab {
        match capture.grab() {
            Ok(()) => println!(
                "GRAB ACTIVE: local pointer and keyboard are NOW SUPPRESSED; recenter threshold {RECENTER_THRESHOLD_PX:.0} px"
            ),
            Err(err) => {
                eprintln!("grab failed, cancelling the transfer instead of passing input through: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!(
            "OBSERVE-ONLY: NOT grabbing. Raw events are printed, but local input is NOT suppressed and the pointer is not recentered."
        );
    }
    println!("manual check 1: press Ctrl+Alt+Shift+F12 while this runs. Expect a line starting with EMERGENCY and a clean exit.");
    println!("manual check 2: confirm the desktop does NOT switch to VT12 / the text console (the chord must not act as XF86Switch_VT_12). If it does, record that in the spike notes.");
    println!("capturing for {} seconds...", options.seconds);

    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut printed = 0u64;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match capture.next_event(Some(remaining.min(Duration::from_millis(250)))) {
            Ok(Some(CapturedEvent::Emergency)) => {
                println!(
                    "EMERGENCY: Ctrl+Alt+Shift+F12 fired; grab released, held input released, exiting cleanly."
                );
                break;
            }
            Ok(Some(CapturedEvent::Input(event))) => {
                printed += 1;
                print_event(&capture, event);
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("capture failed: {err}");
                break;
            }
        }
    }

    println!("events printed: {printed}");
    println!(
        "injected raw events filtered: {}",
        capture.injected_events_filtered()
    );
    println!("recenters: {}", capture.recenter_count());
    println!("emergency events: {}", capture.emergency_events());
    println!("last event deviceid: {}", capture.last_event_deviceid());
    println!(
        "observed ledger: keys={:?} buttons={:?}",
        capture.ledger().keys(),
        capture.ledger().buttons()
    );
    capture.ungrab();
    println!("grab active after ungrab: {}", capture.grab_active());
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
fn print_event(capture: &X11Capture, event: InputEvent) {
    let device = capture.last_event_deviceid();
    match event {
        InputEvent::PointerAbsolute(position) => {
            let (dx, dy) = capture.last_motion_delta();
            println!(
                "device {device}: motion delta ({dx:+.2}, {dy:+.2}) -> integrated ({:.1}, {:.1}) [recenters {}]",
                position.x,
                position.y,
                capture.recenter_count()
            );
        }
        InputEvent::Key { usage, state } => {
            let keycode = input_model::hid_usage_to_evdev_code(usage)
                .and_then(evdev_to_x11_keycode)
                .map(|keycode| format!("{keycode}"))
                .unwrap_or_else(|| "?".to_string());
            println!(
                "device {device}: key HID usage 0x{:02X} (page 0x{:02X}) -> X11 keycode {keycode} {state:?}",
                usage.usage, usage.page
            );
        }
        InputEvent::Button { button, state } => {
            println!("device {device}: button {button:?} {state:?}");
        }
        InputEvent::Scroll { h120, v120 } => {
            println!("device {device}: wheel h120={h120} v120={v120}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("platform-x11 spike: Linux-only; run this on an X11 session.");
}
