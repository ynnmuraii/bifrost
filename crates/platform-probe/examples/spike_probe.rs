//! Session/backend probe spike (KVM-014 / KVM-025).
//!
//! On Linux: gathers the real snapshot (environment, portal versions, X11
//! extensions) and prints the decision for each `[platform].backend` override.
//! On Windows (or with `--synthetic`): runs the same pure decision over synthetic
//! snapshots for laptop A (CachyOS + KDE Plasma 6.7 Wayland), laptop B (Ubuntu
//! 24.04.4 + GNOME 46 Wayland), laptop B's Xorg session, Xwayland, wlroots and a
//! portal older than the minimum, so the refusal messages can be reviewed anywhere.

use platform_probe::env::{EnvSnapshot, HostOs, PortalVersions, X11Extensions};
use platform_probe::{decide, BackendOverride, SessionKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let synthetic = args.iter().any(|a| a == "--synthetic");
    let override_arg = args
        .iter()
        .position(|a| a == "--backend")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "auto".to_owned());

    if let Err(error) = run(synthetic, &override_arg) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run(synthetic: bool, override_arg: &str) -> Result<(), Box<dyn std::error::Error>> {
    let requested = BackendOverride::parse(override_arg)?;

    if synthetic || cfg!(not(target_os = "linux")) {
        if cfg!(not(target_os = "linux")) {
            println!(
                "(non-Linux host: the live probe is unavailable, showing synthetic snapshots)"
            );
        }
        println!("override = {requested}");
        synthetic_scenarios();
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let snapshot = platform_probe::read_snapshot();
        println!("== live snapshot ==");
        println!("  XDG_SESSION_TYPE     = {:?}", snapshot.xdg_session_type);
        println!("  WAYLAND_DISPLAY      = {:?}", snapshot.wayland_display);
        println!("  DISPLAY              = {:?}", snapshot.display);
        println!(
            "  XDG_CURRENT_DESKTOP  = {:?}",
            snapshot.xdg_current_desktop
        );
        println!(
            "  XDG_SESSION_DESKTOP  = {:?}",
            snapshot.xdg_session_desktop
        );
        println!(
            "  impl portal owner    = {:?}",
            snapshot.impl_portal_backend
        );
        println!("  portals queried      = {}", snapshot.portals_queried);
        for (name, version) in snapshot.portals.iter() {
            println!("  portal {name} = {version}");
        }
        println!("  x11 extensions       = {:?}", snapshot.x11);
        println!("  override             = {requested}");
        match decide(&snapshot, requested) {
            Ok(info) => {
                println!("== decision: {} ==", info.backend.name());
                println!("  session_kind    = {}", info.session_kind.as_str());
                println!("  compositor      = {:?}", info.compositor);
                println!("  xwayland        = {}", info.xwayland);
                println!("  capabilities    = {:?}", info.capabilities.advertised());
                println!("  unavailable     = {:?}", info.capabilities.unavailable());
                println!("  BackendInfo     = {:?}", info.to_backend_info());
            }
            Err(error) => println!("== decision: {error} =="),
        }
    }

    Ok(())
}

fn kaby_snapshot() -> EnvSnapshot {
    // CachyOS, KDE Plasma 6.7, Wayland by default.
    EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_DESKTOP", "KDE"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 1,
            remote_desktop: 2,
            clipboard: 1,
            global_shortcuts: 2,
        },
        Some("kde".to_owned()),
    )
}

fn laptop_b_wayland_snapshot() -> EnvSnapshot {
    // Ubuntu 24.04.4, GNOME 46, portal-gnome 46.2: no GlobalShortcuts.
    EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
            ("XDG_SESSION_DESKTOP", "ubuntu"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 1,
            remote_desktop: 2,
            clipboard: 1,
            global_shortcuts: 0,
        },
        Some("gnome".to_owned()),
    )
}

fn laptop_b_xorg_snapshot() -> EnvSnapshot {
    EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "x11"),
            ("DISPLAY", ":0"),
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
            ("XDG_SESSION_DESKTOP", "ubuntu"),
        ],
    )
    .with_x11(X11Extensions {
        xtest: true,
        xi2: true,
        randr: true,
    })
}

fn report(label: &str, snapshot: &EnvSnapshot, requested: BackendOverride) {
    match decide(snapshot, requested) {
        Ok(info) => println!(
            "{label}: OK {} ({}) capabilities={:?}",
            info.backend.name(),
            info.session_kind.as_str(),
            info.capabilities.advertised()
        ),
        Err(error) => println!("{label}: {error}"),
    }
}

fn synthetic_scenarios() {
    println!("== synthetic snapshots ==");
    let kde = kaby_snapshot();
    report("laptop A (KDE Wayland, auto)", &kde, BackendOverride::Auto);
    report(
        "laptop A + backend=x11 (mismatch)",
        &kde,
        BackendOverride::X11,
    );

    let gnome = laptop_b_wayland_snapshot();
    report(
        "laptop B (GNOME 46 Wayland, auto)",
        &gnome,
        BackendOverride::Auto,
    );

    let xorg = laptop_b_xorg_snapshot();
    report(
        "laptop B (Ubuntu on Xorg, auto)",
        &xorg,
        BackendOverride::Auto,
    );
    report(
        "laptop B Xorg + backend=wayland (mismatch)",
        &xorg,
        BackendOverride::Wayland,
    );

    // Xwayland inside a KDE Wayland session, but asking for X11.
    let mut xwayland = kaby_snapshot();
    xwayland.display = Some(":0".to_owned());
    report(
        "KDE Wayland + DISPLAY (Xwayland) + backend=x11",
        &xwayland,
        BackendOverride::X11,
    );
    report(
        "KDE Wayland + DISPLAY (Xwayland), auto",
        &xwayland,
        BackendOverride::Auto,
    );

    let sway = EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XDG_CURRENT_DESKTOP", "sway"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 0,
            remote_desktop: 0,
            clipboard: 0,
            global_shortcuts: 0,
        },
        Some("wlr".to_owned()),
    );
    report("wlroots (sway)", &sway, BackendOverride::Auto);

    let old_portal = EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "GNOME"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 0,
            remote_desktop: 1,
            clipboard: 1,
            global_shortcuts: 0,
        },
        Some("gnome".to_owned()),
    );
    report(
        "GNOME 45 with XDP 1.16 (portal too old)",
        &old_portal,
        BackendOverride::Auto,
    );

    let rd_v1 = EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "KDE"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 1,
            remote_desktop: 1,
            clipboard: 1,
            global_shortcuts: 1,
        },
        Some("kde".to_owned()),
    );
    report(
        "RemoteDesktop v1 (no ConnectToEIS)",
        &rd_v1,
        BackendOverride::Auto,
    );

    let unknown = EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "Lomiri"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 1,
            remote_desktop: 2,
            clipboard: 1,
            global_shortcuts: 1,
        },
        None,
    );
    report("unknown compositor", &unknown, BackendOverride::Auto);

    let tty = EnvSnapshot::from_vars(HostOs::Linux, [("XDG_SESSION_TYPE", "tty")]);
    report("tty", &tty, BackendOverride::Auto);

    let x11_missing = laptop_b_xorg_snapshot().with_x11(X11Extensions {
        xtest: false,
        xi2: true,
        randr: true,
    });
    report("X11 without XTEST", &x11_missing, BackendOverride::Auto);

    let mut xwayland_x11 = laptop_b_xorg_snapshot();
    xwayland_x11.wayland_display = Some("wayland-0".to_owned());
    report(
        "XDG_SESSION_TYPE=x11 with WAYLAND_DISPLAY (Xwayland mix)",
        &xwayland_x11,
        BackendOverride::Auto,
    );

    let windows = EnvSnapshot::new(HostOs::Windows);
    match decide(&windows, BackendOverride::Auto) {
        Ok(info) => println!(
            "Windows (auto): OK {} ({})",
            info.backend.name(),
            info.session_kind.as_str()
        ),
        Err(error) => println!("Windows (auto): {error}"),
    }
    assert_eq!(
        decide(&windows, BackendOverride::Auto)
            .unwrap()
            .session_kind,
        SessionKind::Windows
    );
}
