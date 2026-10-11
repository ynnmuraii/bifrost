//! Spike: inject input through `org.freedesktop.portal.RemoteDesktop` + EIS (KVM-016).
//!
//! What it proves / records:
//!
//! * the portal interface versions and the negotiated EIS protocol version,
//! * the absolute region and the device dimensions the compositor publishes, which is
//!   what the 100/125/150% and fractional-scaling DoD has to be judged against,
//! * that a logical position is either inside the published region (and then lands
//!   exactly there) or is refused instead of being silently discarded by EIS,
//! * that the sub-120 wheel remainder accumulates across events,
//! * spike question 4: whether `ei::Context::connect_to_env()` works without a portal
//!   session (`--direct`).
//!
//! Usage:
//!
//! ```text
//! cargo run -p platform-wayland --example spike_wl_inject -- \
//!     [--seconds 20] [--at X,Y] [--direct]
//! ```
//!
//! Without `--at` the spike draws a small circle around the centre of the first region;
//! with `--at` it only places the cursor once, which is the interesting case for the
//! fractional-scaling check.

use std::process::ExitCode;

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("spike_wl_inject is Linux-only (EIS sender)");
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spike_wl_inject failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    use input_model::{HidUsage, InputEvent, KeyState, Position};
    use platform_wayland::{
        compositor_signature, portal_versions, Compositor, GrantStore, WaylandInjector,
        REMOTE_DESKTOP_INTERFACE,
    };

    struct Args {
        seconds: u64,
        at: Option<Position>,
        direct: bool,
        grant_store: Option<std::path::PathBuf>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            seconds: 20,
            at: None,
            direct: false,
            grant_store: None,
        };
        let mut argv = std::env::args().skip(1);
        while let Some(argument) = argv.next() {
            match argument.as_str() {
                "--seconds" => {
                    let value = argv.next().ok_or("--seconds needs a value")?;
                    args.seconds = value
                        .parse()
                        .map_err(|_| format!("bad --seconds value {value}"))?;
                }
                "--at" => {
                    let value = argv.next().ok_or("--at needs x,y")?;
                    let (x, y) = value.split_once(',').ok_or("--at wants x,y")?;
                    let x: f64 = x
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad --at x in {value}"))?;
                    let y: f64 = y
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad --at y in {value}"))?;
                    args.at = Some(Position::new(x, y));
                }
                "--direct" => args.direct = true,
                "--grant-store" => {
                    args.grant_store = Some(std::path::PathBuf::from(
                        argv.next().ok_or("--grant-store needs a path")?,
                    ));
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(args)
    }

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("spike_wl_inject: {message}");
            eprintln!(
                "usage: spike_wl_inject [--seconds N] [--at X,Y] [--direct] [--grant-store PATH]"
            );
            return Ok(());
        }
    };

    let versions = portal_versions().await;
    println!(
        "portal versions: input_capture={} remote_desktop={} clipboard={} global_shortcuts={}",
        versions.input_capture,
        versions.remote_desktop,
        versions.clipboard,
        versions.global_shortcuts
    );

    // KVM-017: the RemoteDesktop restore token has to survive a restart (and a
    // reboot) so the permission dialog does not come back. The store is keyed by
    // the compositor signature, exactly like the capture side.
    let compositor = Compositor::detect()?;
    let signature = compositor_signature(compositor, &versions);
    let mut store = match &args.grant_store {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => GrantStore::from_toml(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GrantStore::new(),
            Err(error) => return Err(error.into()),
        },
        None => GrantStore::new(),
    };
    println!("compositor: {compositor}");
    println!("grant signature: {signature}");
    println!("grant store entries: {}", store.len());
    let grant = store.get(REMOTE_DESKTOP_INTERFACE, &signature).cloned();
    println!(
        "restore token for injection: {}",
        if grant.is_some() {
            "reused from the store"
        } else {
            "none (the portal will show its dialog)"
        }
    );

    let mut injector = if args.direct {
        println!("--direct: using LIBEI_SOCKET, no portal session and no dialog");
        WaylandInjector::connect_direct().await?
    } else {
        println!("connecting through org.freedesktop.portal.RemoteDesktop");
        WaylandInjector::connect(grant.as_ref()).await?
    };

    println!("ei protocol version: {:?}", injector.ei_protocol_version());
    println!(
        "source: {:?} device: {:?} type: {:?}",
        injector.source(),
        injector.device_name(),
        injector.device_type()
    );
    println!("capabilities: {:?}", injector.capabilities());
    println!("device dimensions (mm): {:?}", injector.dimensions());
    for region in injector.regions() {
        println!(
            "region x={} y={} {}x{} scale={}",
            region.x, region.y, region.width, region.height, region.scale
        );
    }
    println!("absolute region: {:?}", injector.absolute_region());
    println!("backend info: {:?}", injector.backend_info());

    if !injector.capabilities().has_absolute_injection {
        println!(
            "WARNING: no absolute region/interface, so nothing will be injected: relative \
             injection is deliberately not substituted for absolute input"
        );
    }

    let deadline = Duration::from_secs(args.seconds);
    let work = async {
        if let Some(position) = args.at {
            match injector.resolve_absolute(position) {
                Some((x, y)) => {
                    println!("--at {position:?} maps to EIS region coordinates {x},{y}")
                }
                None => println!(
                    "--at {position:?} is outside every region: EIS would discard it silently, so \
                     the spike does not pretend to have moved the cursor"
                ),
            }
            let outcome = injector.inject(&[InputEvent::PointerAbsolute(position)]);
            println!(
                "place cursor: applied={} actual={:?} error={:?}",
                outcome.applied_count,
                outcome.actual,
                outcome.os_error.as_ref().map(ToString::to_string)
            );
        } else if let Some(region) = injector.absolute_region() {
            let centre = Position::new(
                f64::from(region.x) + f64::from(region.width) / 2.0,
                f64::from(region.y) + f64::from(region.height) / 2.0,
            );
            println!("drawing a circle around {centre:?} (region centre)");
            for step in 0..24 {
                let angle = f64::from(step) * std::f64::consts::TAU / 24.0;
                let position = Position::new(
                    centre.x + 120.0 * angle.cos(),
                    centre.y + 120.0 * angle.sin(),
                );
                let outcome = injector.inject(&[InputEvent::PointerAbsolute(position)]);
                match &outcome.os_error {
                    Some(error) => {
                        println!("step {step}: not applied ({error})");
                        break;
                    }
                    None => println!("step {step:>2}: cursor {position:?}"),
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }

        // Two half detents must add up to exactly one detent on the wire.
        for (index, half) in [60i32, 60].into_iter().enumerate() {
            let outcome = injector.inject(&[InputEvent::Scroll {
                h120: half,
                v120: 0,
            }]);
            println!(
                "scroll event {}: {} units applied={} error={:?}",
                index + 1,
                half,
                outcome.applied_count,
                outcome.os_error.as_ref().map(ToString::to_string)
            );
        }

        // A modifier tap: harmless on its own, and it exercises HID -> evdev mapping.
        let shift = HidUsage::keyboard(0xE1);
        for state in [KeyState::Down, KeyState::Up] {
            let outcome = injector.inject(&[InputEvent::Key {
                usage: shift,
                state,
            }]);
            println!(
                "shift {state:?}: applied={} error={:?}",
                outcome.applied_count,
                outcome.os_error.as_ref().map(ToString::to_string)
            );
        }

        // Keep the paused/resumed state current while we wait.
        for _ in 0..10 {
            injector.pump().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        println!(
            "cursor after injection: {:?} (resumed: {})",
            injector.cursor(),
            injector.is_resumed()
        );
    };

    if tokio::time::timeout(deadline, work).await.is_err() {
        println!("{} s timeout reached", args.seconds);
    }

    let released = injector.release_all();
    println!(
        "release_all: {} event(s), error={:?}",
        released.applied_count,
        released.os_error.as_ref().map(ToString::to_string)
    );
    if let Some(error) = injector.take_ei_error() {
        println!("EIS error: {error}");
    }

    // KVM-017 evidence: does the portal hand back a one-shot restore token, and does
    // reusing it suppress the dialog on the next run / after a reboot?
    if let Some(fresh) = injector.grant() {
        println!(
            "portal issued a fresh restore token for {} (not shown)",
            fresh.portal_interface
        );
        store.accept(fresh, &signature);
        if let Some(path) = &args.grant_store {
            std::fs::write(path, store.to_toml())?;
            println!("wrote {} grant(s) to {}", store.len(), path.display());
        }
    } else {
        println!("the portal issued no restore token (persistence unavailable)");
    }

    injector.disconnect().await?;
    println!("disconnected");
    Ok(())
}
