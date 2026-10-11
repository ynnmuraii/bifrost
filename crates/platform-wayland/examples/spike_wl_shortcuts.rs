//! Spike: the emergency hotkey through `org.freedesktop.portal.GlobalShortcuts`
//! (KVM-018).
//!
//! On GNOME 46 the portal does not implement `GlobalShortcuts` at all, and the DoD for
//! this spike is precisely that registration then fails **loudly**, naming the required
//! interface, instead of quietly doing nothing. This example prints the exact error and
//! exits with code 3 for that case, so a runtime script can tell "unavailable" apart
//! from "broken".
//!
//! Usage:
//!
//! ```text
//! cargo run -p platform-wayland --example spike_wl_shortcuts -- \
//!     [--seconds 20] [--trigger CTRL+ALT+SHIFT+F12]
//! ```

use std::process::ExitCode;

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("spike_wl_shortcuts is Linux-only (XDG desktop portal)");
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("spike_wl_shortcuts failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
async fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    use std::time::Duration;

    use platform_wayland::{
        portal_versions, EmergencyShortcuts, WaylandError, GLOBAL_SHORTCUTS_INTERFACE,
    };

    let mut seconds = 20u64;
    let mut trigger = EmergencyShortcuts::DEFAULT_TRIGGER.to_owned();
    let mut argv = std::env::args().skip(1);
    while let Some(argument) = argv.next() {
        match argument.as_str() {
            "--seconds" => {
                let value = argv.next().ok_or("--seconds needs a value")?;
                seconds = value
                    .parse()
                    .map_err(|_| format!("bad --seconds value {value}"))?;
            }
            "--trigger" => {
                trigger = argv.next().ok_or("--trigger needs a value")?;
            }
            other => {
                eprintln!("spike_wl_shortcuts: unknown argument {other}");
                eprintln!("usage: spike_wl_shortcuts [--seconds N] [--trigger TRIGGER]");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }

    let versions = portal_versions().await;
    println!(
        "portal versions: input_capture={} remote_desktop={} clipboard={} global_shortcuts={}",
        versions.input_capture,
        versions.remote_desktop,
        versions.clipboard,
        versions.global_shortcuts
    );
    println!("required portal interface: {GLOBAL_SHORTCUTS_INTERFACE}");
    println!("requested trigger: {trigger}");

    let shortcuts = match EmergencyShortcuts::register(&trigger).await {
        Ok(shortcuts) => shortcuts,
        Err(WaylandError::HotkeyUnavailable { portal_interface }) => {
            println!("HOTKEY UNAVAILABLE: the compositor does not implement {portal_interface}");
            println!(
                "GNOME 46 has no GlobalShortcuts portal (it arrived in xdg-desktop-portal-gnome \
                 48), so this is the expected result there and is reported instead of silently \
                 doing nothing"
            );
            return Ok(ExitCode::from(3));
        }
        Err(error) => return Err(error.into()),
    };

    let mut shortcuts = shortcuts;
    println!(
        "registered: portal version {} bound trigger {:?}",
        shortcuts.portal_version(),
        shortcuts.bound_trigger_description()
    );
    println!("waiting for activations for {seconds} s: press the bound trigger");

    let waiting = async {
        while let Some(shortcut_id) = shortcuts.next_activation().await {
            println!("ACTIVATED shortcut id {shortcut_id}");
        }
    };
    match tokio::time::timeout(Duration::from_secs(seconds), waiting).await {
        Ok(()) => println!("activation stream ended"),
        Err(_elapsed) => println!("{seconds} s timeout reached without an activation"),
    }

    shortcuts.close().await?;
    println!("shortcut session closed");
    Ok(ExitCode::SUCCESS)
}
