use platform_probe::{
    compositor_from_desktop_string, compositor_from_impl_portal, decide, Backend, BackendOverride,
    Compositor, EnvSnapshot, HostOs, PortalVersions, SessionKind, X11Extensions,
};

fn wayland() -> EnvSnapshot {
    EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "GNOME"),
        ],
    )
    .with_portals(
        PortalVersions {
            input_capture: 1,
            remote_desktop: 2,
            clipboard: 0,
            global_shortcuts: 0,
        },
        None,
    )
}

fn x11() -> EnvSnapshot {
    EnvSnapshot::from_vars(
        HostOs::Linux,
        [("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0")],
    )
    .with_x11(X11Extensions {
        xtest: true,
        xi2: true,
        randr: true,
    })
}

#[test]
fn snapshot_normalizes_supplied_variables_without_probing() {
    let snapshot = EnvSnapshot::from_vars(
        HostOs::Linux,
        [
            ("XDG_SESSION_TYPE", "  wayland "),
            ("DISPLAY", " :1 "),
            ("WAYLAND_DISPLAY", " wayland-1 "),
            ("XDG_CURRENT_DESKTOP", "  "),
            ("XDG_SESSION_DESKTOP", " KDE "),
            ("UNRELATED", "ignored"),
        ],
    );
    assert_eq!(snapshot.xdg_session_type.as_deref(), Some("wayland"));
    assert_eq!(snapshot.display.as_deref(), Some(":1"));
    assert_eq!(snapshot.wayland_display.as_deref(), Some("wayland-1"));
    assert_eq!(snapshot.xdg_current_desktop, None);
    assert_eq!(snapshot.xdg_session_desktop.as_deref(), Some("KDE"));
    assert!(snapshot.has_xwayland());
    assert!(!snapshot.portals_queried);
    assert_eq!(snapshot.x11, None);
    assert_eq!(snapshot.portals, PortalVersions::default());
    for vars in [
        vec![],
        vec![("DISPLAY", ":0")],
        vec![("WAYLAND_DISPLAY", "wayland-0")],
    ] {
        assert!(!EnvSnapshot::from_vars(HostOs::Linux, vars).has_xwayland());
    }
}

#[test]
fn overrides_never_cross_session_boundaries() {
    for display in [None, Some(":0".into())] {
        let mut snapshot = wayland();
        snapshot.display = display;
        assert!(decide(&snapshot, BackendOverride::X11).is_err());
        assert!(matches!(
            decide(&snapshot, BackendOverride::Wayland).unwrap().backend,
            Backend::Wayland { .. }
        ));
    }
    assert!(decide(&x11(), BackendOverride::Wayland).is_err());
    assert_eq!(
        decide(&x11(), BackendOverride::X11).unwrap().backend,
        Backend::X11
    );
}

#[test]
fn xwayland_is_wayland_or_refused_never_x11() {
    let mut snapshot = wayland();
    snapshot.display = Some(":0".into());
    snapshot.x11 = x11().x11;
    snapshot.xdg_session_type = None;
    let info = decide(&snapshot, BackendOverride::Auto).unwrap();
    assert_eq!(info.session_kind, SessionKind::Wayland);
    assert!(info.xwayland);
    snapshot.xdg_session_type = Some("x11".into());
    for requested in [
        BackendOverride::Auto,
        BackendOverride::X11,
        BackendOverride::Wayland,
    ] {
        assert!(decide(&snapshot, requested).is_err());
    }
}

#[test]
fn absent_empty_tty_and_unknown_sessions_fail_closed() {
    for session in ["", "  ", "tty", "mir"] {
        let snapshot = EnvSnapshot::from_vars(HostOs::Linux, [("XDG_SESSION_TYPE", session)]);
        assert!(decide(&snapshot, BackendOverride::Auto).is_err());
    }
    assert!(decide(&EnvSnapshot::new(HostOs::Linux), BackendOverride::Auto).is_err());
    let mut snapshot = wayland();
    snapshot.wayland_display = None;
    assert!(decide(&snapshot, BackendOverride::Auto).is_err());
}

#[test]
fn display_fallbacks_select_the_graphical_backend() {
    for session in [None, Some("tty".into())] {
        let mut snapshot = x11();
        snapshot.xdg_session_type = session.clone();
        assert_eq!(
            decide(&snapshot, BackendOverride::Auto).unwrap().backend,
            Backend::X11
        );
        let mut snapshot = wayland();
        snapshot.xdg_session_type = session;
        assert_eq!(
            decide(&snapshot, BackendOverride::Auto)
                .unwrap()
                .session_kind,
            SessionKind::Wayland
        );
    }
}

#[test]
fn portal_minimum_failures_identify_interface_observed_and_required_versions() {
    for (ic, rd, interface, observed, minimum) in [
        (0, 2, PortalVersions::INPUT_CAPTURE, "version 0", ">= 1"),
        (1, 0, PortalVersions::REMOTE_DESKTOP, "version 0", ">= 2"),
        (1, 1, PortalVersions::REMOTE_DESKTOP, "version 1", ">= 2"),
    ] {
        let mut snapshot = wayland();
        snapshot.portals.input_capture = ic;
        snapshot.portals.remote_desktop = rd;
        let error = decide(&snapshot, BackendOverride::Auto).unwrap_err();
        for detail in [interface, observed, minimum] {
            assert!(error.reason.contains(detail), "missing {detail}: {error}");
        }
    }
    assert!(decide(&wayland(), BackendOverride::Auto).is_ok());
}

#[test]
fn unsupported_compositors_and_unreachable_bus_are_refused() {
    for desktop in ["Sway", "Hyprland", "COSMIC", "MATE", "unknown-desktop"] {
        let mut snapshot = wayland();
        snapshot.xdg_current_desktop = Some(desktop.into());
        let error = decide(&snapshot, BackendOverride::Auto).unwrap_err();
        assert!(error.reason.contains(desktop));
    }
    let mut snapshot = wayland();
    snapshot.portals_queried = false;
    assert!(decide(&snapshot, BackendOverride::Auto).is_err());
}

#[test]
fn every_missing_x11_extension_combination_is_refused() {
    for mask in 0..8 {
        let extensions = X11Extensions {
            xtest: mask & 1 != 0,
            xi2: mask & 2 != 0,
            randr: mask & 4 != 0,
        };
        let expected: Vec<_> = [(1, "XTEST"), (2, "XInput2"), (4, "RANDR")]
            .into_iter()
            .filter(|(bit, _)| mask & bit == 0)
            .map(|(_, name)| name)
            .collect();
        assert_eq!(extensions.missing(), expected);
        assert_eq!(extensions.all_present(), mask == 7);
        let mut snapshot = x11();
        snapshot.x11 = Some(extensions);
        let result = decide(&snapshot, BackendOverride::Auto);
        if mask == 7 {
            assert!(result.is_ok());
        } else {
            let error = result.unwrap_err();
            for name in expected {
                assert!(error.reason.contains(name));
            }
        }
    }
    let mut snapshot = x11();
    snapshot.x11 = None;
    assert!(decide(&snapshot, BackendOverride::Auto).is_err());
}

#[test]
fn windows_ignores_linux_session_data_but_rejects_linux_overrides() {
    let mut snapshot = wayland();
    snapshot.host = HostOs::Windows;
    let info = decide(&snapshot, BackendOverride::Auto).unwrap();
    assert_eq!(info.backend, Backend::Windows);
    assert_eq!(info.session_kind, SessionKind::Windows);
    for requested in [BackendOverride::X11, BackendOverride::Wayland] {
        assert!(decide(&snapshot, requested).is_err());
    }
}

#[test]
fn capabilities_and_backend_info_expose_only_available_features() {
    for shortcuts in [0, 1, u32::MAX] {
        let mut snapshot = wayland();
        snapshot.portals.global_shortcuts = shortcuts;
        snapshot.portals.clipboard = 1;
        let session = decide(&snapshot, BackendOverride::Auto).unwrap();
        let caps = session.capabilities;
        assert_eq!(caps.emergency_hotkey, shortcuts > 0);
        assert!(!caps.receiver_takeover);
        assert!(!caps.text_clipboard);
        assert!(caps.absolute_pointer && caps.capture && caps.inject && caps.physical_hid_keys);
        assert!(!caps.native_repeat && !caps.software_repeat);
        let advertised = caps.advertised();
        assert_eq!(advertised.contains(&"EMERGENCY_HOTKEY"), shortcuts > 0);
        assert!(advertised.contains(&"ABSOLUTE_POINTER"));
        assert!(!advertised.contains(&"TEXT_CLIPBOARD"));
        assert!(!advertised.contains(&"RECEIVER_TAKEOVER"));
        assert_eq!(
            caps.unavailable().contains(&"EMERGENCY_HOTKEY"),
            shortcuts == 0
        );
        assert!(caps.unavailable().contains(&"TEXT_CLIPBOARD"));
        assert!(caps.unavailable().contains(&"RECEIVER_TAKEOVER"));
        let info = session.to_backend_info();
        assert_eq!(info.backend, "platform-wayland");
        assert_eq!(info.compositor, "mutter");
        assert_eq!(info.has_emergency_hotkey, shortcuts > 0);
        assert!(
            info.has_capture
                && info.has_inject
                && info.has_absolute_injection
                && info.has_portal_persistence
        );
        assert!(!info.has_receiver_takeover);
        assert_eq!(info.portals.len(), if shortcuts == 0 { 3 } else { 4 });
        assert!(info.portals.iter().all(|portal| portal.version > 0));
    }
    for snapshot in [x11(), EnvSnapshot::new(HostOs::Windows)] {
        let session = decide(&snapshot, BackendOverride::Auto).unwrap();
        assert!(session.capabilities.receiver_takeover && session.capabilities.emergency_hotkey);
        assert!(!session.capabilities.text_clipboard);
        assert_eq!(session.capabilities.unavailable(), ["TEXT_CLIPBOARD"]);
        let info = session.to_backend_info();
        assert!(info.portals.is_empty());
        assert!(info.compositor.is_empty());
        assert!(!info.has_portal_persistence);
    }
}

#[test]
fn desktop_and_portal_identity_respect_precedence_and_fallback() {
    for (name, expected) in [
        (" ubuntu:GNOME ", Compositor::Gnome),
        ("ubuntu:PLASMA", Compositor::Kde),
        ("unknown: KDE", Compositor::Kde),
        ("ubuntu", Compositor::Gnome),
    ] {
        assert_eq!(compositor_from_desktop_string(name), Some(expected));
    }
    assert_eq!(compositor_from_desktop_string("unknown::"), None);
    assert_eq!(compositor_from_impl_portal(" KDE "), Some(Compositor::Kde));
    assert_eq!(compositor_from_impl_portal("unknown"), None);
    let mut snapshot = wayland();
    snapshot.impl_portal_backend = Some("kde".into());
    assert_eq!(
        decide(&snapshot, BackendOverride::Auto).unwrap().compositor,
        Some(Compositor::Kde)
    );
    snapshot.impl_portal_backend = Some("unrecognized".into());
    snapshot.xdg_current_desktop = Some("unknown".into());
    snapshot.xdg_session_desktop = Some("plasma".into());
    assert_eq!(
        decide(&snapshot, BackendOverride::Auto).unwrap().compositor,
        Some(Compositor::Kde)
    );
}

#[test]
fn override_parser_accepts_config_values_and_rejects_unknown_values() {
    for (value, expected) in [
        ("", BackendOverride::Auto),
        (" AuTo ", BackendOverride::Auto),
        (" X11 ", BackendOverride::X11),
        ("WAYLAND", BackendOverride::Wayland),
    ] {
        assert_eq!(BackendOverride::parse(value), Ok(expected));
    }
    assert!(BackendOverride::parse("windows").is_err());
    assert!(BackendOverride::parse("xwayland").is_err());
}

#[test]
fn portal_versions_preserve_names_values_and_absence() {
    let versions = PortalVersions {
        input_capture: 1,
        remote_desktop: 2,
        clipboard: 0,
        global_shortcuts: 4,
    };
    let expected = [
        (PortalVersions::INPUT_CAPTURE, 1),
        (PortalVersions::REMOTE_DESKTOP, 2),
        (PortalVersions::CLIPBOARD, 0),
        (PortalVersions::GLOBAL_SHORTCUTS, 4),
    ];
    assert_eq!(versions.iter().collect::<Vec<_>>(), expected);
    for (name, version) in expected {
        assert_eq!(versions.get(name), version);
    }
    assert_eq!(versions.get("org.freedesktop.portal.Unknown"), 0);
}

#[test]
fn decisions_are_repeatable_and_do_not_mutate_snapshots() {
    for snapshot in [
        wayland(),
        x11(),
        EnvSnapshot::new(HostOs::Linux),
        EnvSnapshot::new(HostOs::Windows),
    ] {
        let original = snapshot.clone();
        for requested in [
            BackendOverride::Auto,
            BackendOverride::X11,
            BackendOverride::Wayland,
        ] {
            let first = decide(&snapshot, requested);
            let _ = decide(&EnvSnapshot::new(HostOs::Linux), BackendOverride::Auto);
            assert_eq!(decide(&snapshot, requested), first);
            assert_eq!(snapshot, original);
        }
    }
}
