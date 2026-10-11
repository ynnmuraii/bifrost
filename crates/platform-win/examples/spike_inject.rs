//! KVM-011 spike: synthetic input through `SendInput`.
//!
//! Injects a safe, deterministic sequence into whatever window currently has
//! focus: the physical HID usages for `lws test 123` (letters, digits and spaces
//! only — no Enter, no clicks, no clipboard, nothing destructive), then releases
//! everything it pressed. No key text is ever printed; only usages and scancodes.
//!
//! ```text
//! cargo run -p platform-win --example spike_inject -- --dry-run
//! cargo run -p platform-win --example spike_inject -- --countdown 5
//! cargo run -p platform-win --example spike_inject -- --countdown 5 --partial-demo
//! cargo run -p platform-win --example spike_inject -- --countdown 5 --hold-demo
//! ```
//!
//! UIPI: if the focused window belongs to a higher-integrity process (an elevated
//! or UAC prompt window), Windows refuses the injection. The spike reports that
//! as `ACCESS_DENIED`; it never retries and never hides it.

#[cfg(not(windows))]
fn main() {
    eprintln!("spike_inject is Windows-only (KVM-011 injects with SendInput)");
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use std::process::ExitCode;
    use std::time::Duration;

    use input_model::{HidKey, InputEvent, KeyState};
    use platform_win::dpi;
    use platform_win::inject::{describe_event, describe_record, InjectStatus, Injector};

    /// A usage that is deliberately absent from the scancode table, used by
    /// `--partial-demo` to show the "which events were not injected" accounting.
    const UNMAPPED_USAGE: u16 = 0xFFFF;

    struct Args {
        dry_run: bool,
        countdown: u64,
        partial_demo: bool,
        hold_demo: bool,
        do_move: bool,
    }

    pub fn run() -> ExitCode {
        let args = match parse_args() {
            Ok(args) => args,
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::from(2);
            }
        };

        let awareness = dpi::ensure_per_monitor_v2();
        println!("== lws spike_inject (KVM-011) ==");
        println!(
            "dpi awareness: {awareness} {}",
            if awareness == dpi::DpiAwareness::PerMonitorV2 {
                "[OK: PerMonitorV2, absolute coordinates are physical pixels]"
            } else {
                "[WARN: not PerMonitorV2, absolute positioning may use virtualised coordinates]"
            }
        );

        let desktop = Injector::virtual_desktop();
        println!(
            "virtual desktop: left={} top={} width={} height={}",
            desktop.left, desktop.top, desktop.width, desktop.height
        );

        let mut events = tap_sequence();
        if args.partial_demo {
            // Deliberately unrepresentable: proves the accounting is per event.
            events.insert(
                events.len() / 2,
                InputEvent::Key {
                    key: HidKey::keyboard(UNMAPPED_USAGE),
                    state: KeyState::Down,
                },
            );
        }
        if args.do_move {
            events.push(InputEvent::PointerRelative { dx: 8, dy: 8 });
            events.push(InputEvent::PointerRelative { dx: -8, dy: -8 });
        }

        let plan = Injector::plan_for(&events);
        println!(
            "batch: {} events -> {} INPUT records ({} unmapped, {} motion)",
            events.len(),
            plan.record_count(),
            plan.unmapped_count(),
            plan.motion_count()
        );
        for (index, record) in plan.records.iter().enumerate() {
            println!("  [{index:>2}] {}", describe_record(*record));
        }

        if args.dry_run {
            println!(
                "--dry-run: nothing was injected ({} events planned)",
                events.len()
            );
            return ExitCode::SUCCESS;
        }

        println!();
        println!("focus the window that may receive the test text (nothing dangerous is typed:");
        println!(
            "12 key taps, no Enter, no clicks). Injecting in {}s.",
            args.countdown
        );
        for remaining in (1..=args.countdown).rev() {
            println!("  {remaining}...");
            std::thread::sleep(Duration::from_secs(1));
        }

        let mut injector = Injector::new();
        let status = injector.inject(&events);
        report(&status);

        if args.hold_demo {
            println!("--hold-demo: holding Left Shift for 1.5s, then releasing everything");
            let hold = injector.inject(&[InputEvent::Key {
                key: HidKey::keyboard(0xE1),
                state: KeyState::Down,
            }]);
            report(&hold);
            std::thread::sleep(Duration::from_millis(1500));
        }

        println!(
            "ledger before cleanup: {} keys, {} buttons held",
            injector.ledger().key_count(),
            injector.ledger().button_count()
        );
        let cleanup = injector.release_all();
        report(&cleanup);
        println!(
            "ledger after cleanup: {} keys, {} buttons held",
            injector.ledger().key_count(),
            injector.ledger().button_count()
        );

        println!();
        println!("UIPI/UAC check (manual): focus an elevated window (an Administrator console)");
        println!(
            "and rerun; SendInput must report ACCESS_DENIED instead of silently doing nothing."
        );

        ExitCode::SUCCESS
    }

    /// Press/release pairs for the physical positions of `lws test 123`.
    ///
    /// The usages are the HID keyboard-page ids of the key positions: `l`=0x0F,
    /// `w`=0x1A, `s`=0x16, space=0x2C, `t`=0x17, `e`=0x08, `1`=0x1E, `2`=0x1F,
    /// `3`=0x20. Which character these produce is the receiver's layout's business.
    fn tap_sequence() -> Vec<InputEvent> {
        const USAGES: [u16; 12] = [
            0x0F, 0x1A, 0x16, 0x2C, 0x17, 0x08, 0x16, 0x17, 0x2C, 0x1E, 0x1F, 0x20,
        ];

        let mut events = Vec::with_capacity(USAGES.len() * 2);
        for usage in USAGES {
            let key = HidKey::keyboard(usage);
            events.push(InputEvent::Key {
                key,
                state: KeyState::Down,
            });
            events.push(InputEvent::Key {
                key,
                state: KeyState::Up,
            });
        }
        events
    }

    fn report(status: &InjectStatus) {
        println!("  result: {status}");
        for event in &status.not_injected {
            println!("  not injected: {}", describe_event(event));
        }
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            dry_run: false,
            countdown: 5,
            partial_demo: false,
            hold_demo: false,
            do_move: false,
        };

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--dry-run" => args.dry_run = true,
                "--countdown" => args.countdown = next_u64(&mut iter, "--countdown")?.min(60),
                "--partial-demo" => args.partial_demo = true,
                "--hold-demo" => args.hold_demo = true,
                "--move" => args.do_move = true,
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
        "usage: spike_inject [--dry-run] [--countdown N] [--partial-demo] [--hold-demo] [--move]"
            .to_owned()
    }
}
