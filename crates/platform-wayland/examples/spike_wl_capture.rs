//! Spike: capture input through `org.freedesktop.portal.InputCapture` (KVM-015) plus
//! the emergency hotkey path (KVM-018).
//!
//! What it proves / records:
//!
//! * the portal interface versions and the negotiated EIS protocol version,
//! * the zones the compositor offers and which barriers it refuses,
//! * whether the EIS receiver delivers relative deltas, absolute positions or both
//!   (spike question 3),
//! * that a capture session can be released back to the compositor (`Release`),
//! * that the emergency hotkey, when the compositor has one, ends capture and releases
//!   input instead of leaving the keyboard captured (on GNOME 46 there is no
//!   `GlobalShortcuts` portal, and the spike says so).
//!
//! Usage:
//!
//! ```text
//! cargo run -p platform-wayland --example spike_wl_capture -- \
//!     [--seconds 20] [--grant-store ~/.local/share/lan-workspace/grants.toml]
//! ```
//!
//! Move the pointer to the **top edge** of a monitor: that is where the spike puts its
//! barriers. Press `Ctrl+Alt+Shift+F12` (if the compositor supports the hotkey) or
//! `Ctrl+C` to quit before the timeout.

use std::process::ExitCode;

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("spike_wl_capture is Linux-only (XDG desktop portal + EIS)");
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spike_wl_capture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn report(event: &platform_wayland::CapturedEvent) -> bool {
    use input_model::InputEvent;
    use platform_wayland::CapturedEvent;

    match event {
        CapturedEvent::Input(InputEvent::PointerAbsolute(position)) => {
            println!("pointer {:>10.3} {:>10.3}", position.x, position.y);
            false
        }
        CapturedEvent::Input(other) => {
            println!("input {other:?}");
            false
        }
        CapturedEvent::Activated {
            activation_id,
            cursor,
            barrier_id,
        } => {
            println!("ACTIVATED id={activation_id} barrier={barrier_id:?} cursor={cursor:?}");
            false
        }
        CapturedEvent::Deactivated => {
            println!("DEACTIVATED (input is the compositor's again)");
            false
        }
        CapturedEvent::ZonesChanged { zone_set } => {
            println!("zones changed -> zone set {zone_set} (barriers need a new session)");
            false
        }
        CapturedEvent::Emergency => {
            println!("EMERGENCY: dropping queued input and releasing the session");
            true
        }
    }
}

#[cfg(target_os = "linux")]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::time::Duration;

    use input_model::Position;
    use platform_wayland::{
        compositor_signature, portal_versions, Barrier, BarrierID, BarrierPosition, CapturedEvent,
        Compositor, EmergencyShortcuts, GrantStore, WaylandCapture, WaylandError,
        INPUT_CAPTURE_INTERFACE,
    };

    struct Args {
        seconds: u64,
        grant_store: Option<PathBuf>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            seconds: 20,
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
                "--grant-store" => {
                    args.grant_store = Some(PathBuf::from(
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
            eprintln!("spike_wl_capture: {message}");
            eprintln!("usage: spike_wl_capture [--seconds N] [--grant-store PATH]");
            return Ok(());
        }
    };

    let compositor = Compositor::detect()?;
    let versions = portal_versions().await;
    let signature = compositor_signature(compositor, &versions);
    println!("compositor: {compositor}");
    println!(
        "portal versions: input_capture={} remote_desktop={} clipboard={} global_shortcuts={}",
        versions.input_capture,
        versions.remote_desktop,
        versions.clipboard,
        versions.global_shortcuts
    );
    println!("grant signature: {signature}");

    let mut store = match &args.grant_store {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => GrantStore::from_toml(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GrantStore::new(),
            Err(error) => return Err(error.into()),
        },
        None => GrantStore::new(),
    };
    println!("grant store entries: {}", store.len());
    let grant = store.get(INPUT_CAPTURE_INTERFACE, &signature).cloned();
    println!(
        "restore token for capture: {}",
        if grant.is_some() {
            "reused from the store"
        } else {
            "none (the portal will show its dialog)"
        }
    );

    let mut capture = WaylandCapture::connect(grant.as_ref()).await?;
    println!("ei protocol version: {:?}", capture.ei_protocol_version());
    println!(
        "granted capabilities: {:?} (clipboard enabled: {})",
        capture.capabilities(),
        capture.clipboard_enabled()
    );
    println!("zones: {:?}", capture.zones());
    println!("zone set: {}", capture.zone_set());
    println!("available portals: {:?}", capture.available_portals());

    let barriers: Vec<Barrier> = capture
        .zones()
        .iter()
        .enumerate()
        .filter_map(|(index, region)| {
            let id = BarrierID::new(u32::try_from(index + 1).ok()?)?;
            let x = region.x_offset();
            let y = region.y_offset();
            let width = i32::try_from(region.width()).ok()?;
            Some(Barrier::new(
                id,
                BarrierPosition::new(x, y, x + width - 1, y),
            ))
        })
        .collect();
    println!(
        "requested {} barriers on the top edge of each zone",
        barriers.len()
    );
    let failed = capture.set_barriers(&barriers).await?;
    println!("barriers the compositor rejected: {failed:?}");

    capture.enable().await?;
    println!("capture enabled");

    let mut shortcuts = match EmergencyShortcuts::register(EmergencyShortcuts::DEFAULT_TRIGGER)
        .await
    {
        Ok(shortcuts) => {
            println!(
                "emergency hotkey registered, the compositor calls it {:?}",
                shortcuts.bound_trigger_description()
            );
            Some(shortcuts)
        }
        Err(WaylandError::HotkeyUnavailable { portal_interface }) => {
            println!(
                "emergency hotkey unavailable: {portal_interface} is missing (expected on GNOME 46); \
                 use Ctrl+C to quit"
            );
            None
        }
        Err(error) => return Err(error.into()),
    };
    let emergency = capture.emergency_handle();

    println!(
        "move the pointer to the TOP edge of a monitor to capture input (timeout {} s)",
        args.seconds
    );

    let mut emergency_raised = false;
    let mut hotkey_dead = false;
    let events = async {
        loop {
            // Rebuilt every round: either the live shortcut stream, or a future that
            // never resolves (no hotkey on this compositor, or the session ended).
            let hotkey = async {
                match shortcuts.as_mut() {
                    Some(shortcuts) => shortcuts.next_activation().await,
                    None => std::future::pending::<Option<String>>().await,
                }
            };
            tokio::select! {
                event = capture.next_event() => {
                    let Some(event) = event else { break };
                    if report(&event) {
                        emergency_raised = matches!(event, CapturedEvent::Emergency);
                        break;
                    }
                }
                activated = hotkey => match activated {
                    Some(shortcut_id) => {
                        println!("emergency hotkey {shortcut_id} activated");
                        emergency.trigger();
                    }
                    None => {
                        println!("emergency shortcut session ended; continuing without it");
                        hotkey_dead = true;
                    }
                }
            }
            if hotkey_dead {
                hotkey_dead = false;
                shortcuts = None;
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(args.seconds), events).await {
        Ok(()) => println!("event loop finished"),
        Err(_elapsed) => println!("{} s timeout reached", args.seconds),
    }

    if emergency_raised {
        println!("dropped {} queued event(s)", capture.discard_buffer());
    }
    println!("motion counts: {:?}", capture.motion_counts());
    println!(
        "dropped: unseeded_relative={} unmapped={} buffered_overflow={}",
        capture.dropped_unseeded_motion_count(),
        capture.unmapped_input_count(),
        capture.buffered_dropped_count()
    );
    println!("session still enabled: {}", capture.is_enabled());
    if let Some(error) = capture.take_ei_error() {
        println!("EIS error: {error}");
    }

    match capture.cursor() {
        Some(cursor) if capture.is_active() => {
            // Nudge past the barrier we came in through, so the local cursor keeps moving.
            let nudge = Position::new(cursor.x + 1.0, cursor.y + 1.0);
            capture.release(nudge).await?;
            println!("released input, local cursor continues at {nudge:?}");
        }
        Some(cursor) => println!("capture not active; cursor was at {cursor:?}"),
        None => println!("no absolute position was ever established"),
    }

    if let Some(shortcuts) = &shortcuts {
        shortcuts.close().await?;
    }

    if let Some(grant) = capture.grant() {
        println!(
            "portal issued a fresh restore token for {} (not shown)",
            grant.portal_interface
        );
        store.accept(grant, &signature);
        if let Some(path) = &args.grant_store {
            std::fs::write(path, store.to_toml())?;
            println!("wrote {} grant(s) to {}", store.len(), path.display());
        }
    } else {
        println!("the portal issued no restore token (persistence unavailable)");
    }

    match capture.close().await {
        Ok(()) => println!("session closed"),
        Err(WaylandError::Portal(error)) => println!("session close reported: {error}"),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
