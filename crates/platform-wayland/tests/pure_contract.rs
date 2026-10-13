#![cfg(target_os = "linux")]

use input_model::Position;
use platform_wayland::{
    compositor_signature, portal_interfaces, Compositor, GrantStore, PortalGrant, PortalVersions,
    Region, WaylandError, CLIPBOARD_INTERFACE, GLOBAL_SHORTCUTS_INTERFACE, INPUT_CAPTURE_INTERFACE,
    REMOTE_DESKTOP_INTERFACE,
};

#[test]
fn desktop_lists_recognize_supported_compositors_without_environment_access() {
    for names in [
        "GNOME",
        " ubuntu:GNOME ",
        "unknown:gnome-classic",
        "pop",
        "gnome-flashback",
        "gnome-xorg",
    ] {
        assert_eq!(
            Compositor::from_desktop_names(names),
            Some(Compositor::Gnome)
        );
    }
    for names in ["KDE", "unknown: plasma ", "PLASMAWAYLAND"] {
        assert_eq!(Compositor::from_desktop_names(names), Some(Compositor::Kde));
    }
    for names in ["", "::", "sway", "Hyprland", "not-gnome"] {
        assert_eq!(Compositor::from_desktop_names(names), None);
    }
}

#[test]
fn signatures_change_for_each_portal_version_and_compositor() {
    let versions = PortalVersions {
        input_capture: 1,
        remote_desktop: 2,
        clipboard: 3,
        global_shortcuts: 4,
    };
    let signature = compositor_signature(Compositor::Gnome, &versions);
    assert_eq!(
        compositor_signature(Compositor::Gnome, &versions),
        signature
    );
    assert_ne!(compositor_signature(Compositor::Kde, &versions), signature);
    for changed in [
        PortalVersions {
            input_capture: 0,
            ..versions
        },
        PortalVersions {
            remote_desktop: 3,
            ..versions
        },
        PortalVersions {
            clipboard: u32::MAX,
            ..versions
        },
        PortalVersions {
            global_shortcuts: 0,
            ..versions
        },
    ] {
        assert_ne!(compositor_signature(Compositor::Gnome, &changed), signature);
    }
}

#[test]
fn absent_portal_interfaces_are_omitted_for_every_combination() {
    for mask in 0..16 {
        let versions = PortalVersions {
            input_capture: u32::from(mask & 1 != 0),
            remote_desktop: u32::from(mask & 2 != 0),
            clipboard: u32::from(mask & 4 != 0),
            global_shortcuts: u32::from(mask & 8 != 0),
        };
        let expected: Vec<_> = [
            (1, INPUT_CAPTURE_INTERFACE),
            (2, REMOTE_DESKTOP_INTERFACE),
            (4, CLIPBOARD_INTERFACE),
            (8, GLOBAL_SHORTCUTS_INTERFACE),
        ]
        .into_iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| name)
        .collect();
        assert_eq!(portal_interfaces(&versions), expected);
        assert_eq!(versions.all_present(), mask == 15);
    }
}

#[test]
fn grants_replace_tokens_and_remove_stale_or_empty_entries() {
    let mut store = GrantStore::new();
    assert!(store.is_empty());
    for token in ["first", "replacement"] {
        assert!(store.accept(
            PortalGrant::new(INPUT_CAPTURE_INTERFACE, token, "endpoint"),
            "endpoint"
        ));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .get(INPUT_CAPTURE_INTERFACE, "endpoint")
                .unwrap()
                .token(),
            token
        );
    }
    assert!(!store.accept(
        PortalGrant::new(INPUT_CAPTURE_INTERFACE, "", "endpoint"),
        "endpoint"
    ));
    assert!(store.is_empty());
    assert!(store.accept(
        PortalGrant::new(INPUT_CAPTURE_INTERFACE, "token", "endpoint"),
        "endpoint"
    ));
    assert!(!store.accept(
        PortalGrant::new(INPUT_CAPTURE_INTERFACE, "stale", "old-endpoint"),
        "endpoint"
    ));
    assert!(store.is_empty());
    assert!(store.accept(
        PortalGrant::new(INPUT_CAPTURE_INTERFACE, "token", "endpoint"),
        "endpoint"
    ));
    assert!(store.get(INPUT_CAPTURE_INTERFACE, "new-endpoint").is_none());
    assert!(store.get(INPUT_CAPTURE_INTERFACE, "endpoint").is_none());
    assert!(store.is_empty());
}

#[test]
fn accepting_a_new_endpoint_invalidates_all_old_endpoint_grants() {
    let mut store = GrantStore::new();
    for interface in [INPUT_CAPTURE_INTERFACE, REMOTE_DESKTOP_INTERFACE] {
        assert!(store.accept(PortalGrant::new(interface, "old-token", "old"), "old"));
    }
    assert!(store.accept(
        PortalGrant::new(GLOBAL_SHORTCUTS_INTERFACE, "new-token", "new"),
        "new"
    ));
    assert_eq!(store.len(), 1);
    assert!(store.get(INPUT_CAPTURE_INTERFACE, "old").is_none());
    assert!(store.get(REMOTE_DESKTOP_INTERFACE, "old").is_none());
    assert_eq!(
        store
            .get(GLOBAL_SHORTCUTS_INTERFACE, "new")
            .unwrap()
            .token(),
        "new-token"
    );
}

#[test]
fn grants_can_be_retained_removed_and_cleared_idempotently() {
    let mut store = GrantStore::new();
    for interface in [REMOTE_DESKTOP_INTERFACE, INPUT_CAPTURE_INTERFACE] {
        assert!(store.accept(PortalGrant::new(interface, "token", "endpoint"), "endpoint"));
    }
    assert_eq!(
        store
            .iter()
            .map(|grant| grant.portal_interface.as_str())
            .collect::<Vec<_>>(),
        [INPUT_CAPTURE_INTERFACE, REMOTE_DESKTOP_INTERFACE]
    );
    assert_eq!(store.retain_signature("endpoint"), 0);
    assert_eq!(
        store.remove(INPUT_CAPTURE_INTERFACE).unwrap().token(),
        "token"
    );
    assert!(store.remove(INPUT_CAPTURE_INTERFACE).is_none());
    assert_eq!(store.retain_signature("changed"), 1);
    assert_eq!(store.retain_signature("changed"), 0);
    assert!(store.is_empty());
    assert!(store.accept(
        PortalGrant::new(INPUT_CAPTURE_INTERFACE, "token", "endpoint"),
        "endpoint"
    ));
    store.clear();
    store.clear();
    assert_eq!(store.len(), 0);
    assert!(store.is_empty());
}

#[test]
fn grant_toml_round_trips_escaped_and_unicode_fields() {
    let mut store = GrantStore::new();
    let signature = "gnome;quoted=\"yes\";path=C:\\portal\n\r\t\u{0001}";
    for (interface, token) in [
        ("portal.\"one\"", "secret\\path\nline\r\ttab\u{0000}é🔑"),
        ("portal.two", "another token"),
    ] {
        assert!(store.accept(PortalGrant::new(interface, token, signature), signature));
    }
    assert_eq!(GrantStore::from_toml(&store.to_toml()).unwrap(), store);
    assert_eq!(
        GrantStore::from_toml(&GrantStore::new().to_toml()).unwrap(),
        GrantStore::new()
    );
}

#[test]
fn grant_and_store_debug_never_disclose_tokens() {
    let token = "sensitive-restore-token-7fbd";
    let grant = PortalGrant::new(INPUT_CAPTURE_INTERFACE, token, "endpoint");
    assert!(grant.has_token());
    for debug in [format!("{grant:?}"), format!("{grant:#?}")] {
        assert!(!debug.contains(token));
    }
    let mut store = GrantStore::new();
    assert!(store.accept(grant, "endpoint"));
    for debug in [format!("{store:?}"), format!("{store:#?}")] {
        assert!(!debug.contains(token));
    }
}

fn record(interface: &str, token_literal: &str, signature: &str) -> String {
    format!("[[grant]]\nportal_interface = \"{interface}\"\nrestore_token = {token_literal}\ncompositor_signature = \"{signature}\"\n")
}

#[test]
fn malformed_grant_records_return_invalid_grant() {
    for malformed in [
        "portal_interface = \"outside\"",
        "[[unknown]]",
        "[[grant]]",
        "[[grant]]\nunknown = \"value\"",
        "[[grant]]\nportal_interface = \"unterminated",
        "[[grant]]\nportal_interface = 1",
        "[[grant]]\nportal_interface = \"p\"\nrestore_token = \"t\"",
    ] {
        assert!(
            matches!(
                GrantStore::from_toml(malformed),
                Err(WaylandError::InvalidGrant(_))
            ),
            "{malformed}"
        );
    }
    for malformed in [
        record("", "\"token\"", "endpoint"),
        record("p", "\"token\"", ""),
        record("p", "\"\\q\"", "endpoint"),
        record("p", "\"\\uD800\"", "endpoint"),
    ] {
        assert!(matches!(
            GrantStore::from_toml(&malformed),
            Err(WaylandError::InvalidGrant(_))
        ));
    }
}

#[test]
fn grant_parser_rejects_unescaped_quotes_inside_basic_strings() {
    let malformed = record("portal", "\"before\"after\"", "endpoint");
    assert!(matches!(
        GrantStore::from_toml(&malformed),
        Err(WaylandError::InvalidGrant(_))
    ));
}

#[test]
fn loading_grants_does_not_create_a_store_with_multiple_signatures() {
    let text = record(INPUT_CAPTURE_INTERFACE, "\"first\"", "old")
        + &record(REMOTE_DESKTOP_INTERFACE, "\"second\"", "new");
    // Either refusing mixed endpoints or retaining one endpoint satisfies the
    // store invariant; handing out both endpoints' credentials does not.
    if let Ok(store) = GrantStore::from_toml(&text) {
        let signatures: std::collections::BTreeSet<_> = store
            .iter()
            .map(|grant| grant.compositor_signature.as_str())
            .collect();
        assert!(signatures.len() <= 1);
    }
}

#[test]
fn empty_token_records_are_skipped_without_hiding_valid_records() {
    let text = record(INPUT_CAPTURE_INTERFACE, "\"\"", "endpoint")
        + &record(REMOTE_DESKTOP_INTERFACE, "\"valid\"", "endpoint");
    let mut store = GrantStore::from_toml(&text).unwrap();
    assert_eq!(store.len(), 1);
    assert!(store.get(INPUT_CAPTURE_INTERFACE, "endpoint").is_none());
    assert_eq!(
        store
            .get(REMOTE_DESKTOP_INTERFACE, "endpoint")
            .unwrap()
            .token(),
        "valid"
    );
}

#[test]
fn eis_regions_preserve_logical_geometry_and_fractional_scale() {
    let advertised = reis::event::Region {
        x: 1920,
        y: 100,
        width: 1280,
        height: 720,
        scale: 1.5,
        mapping_id: Some("display-2".into()),
    };
    let region = Region::from_ei(&advertised);
    assert_eq!(
        region,
        Region {
            x: 1920,
            y: 100,
            width: 1280,
            height: 720,
            scale: 1.5
        }
    );
    assert!(region.contains(Position::new(1920.0, 100.0)));
    assert!(region.contains(Position::new(3199.5, 819.5)));
    for position in [
        Position::new(1919.5, 100.0),
        Position::new(1920.0, 99.5),
        Position::new(3200.0, 100.0),
        Position::new(1920.0, 820.0),
    ] {
        assert!(!region.contains(position));
    }
}

#[test]
fn laptop_fractional_scale_keeps_region_bounds_in_logical_units() {
    let region = Region::from_ei(&reis::event::Region {
        x: 0,
        y: 0,
        width: 1745,
        height: 1091,
        scale: 1.1,
        mapping_id: None,
    });
    // These logical positions remain valid even though multiplying them by the
    // compositor's physical scale would put them outside the advertised region.
    for position in [
        Position::new(0.0, 0.0),
        Position::new(1600.0, 1000.0),
        Position::new(1744.5, 1090.5),
    ] {
        assert!(region.contains(position), "{position:?}");
    }
    // Bounds are half-open and must not be expanded to physical dimensions.
    for position in [
        Position::new(-0.5, 0.0),
        Position::new(0.0, -0.5),
        Position::new(1745.0, 0.0),
        Position::new(0.0, 1091.0),
        Position::new(1800.0, 1000.0),
        Position::new(1600.0, 1150.0),
    ] {
        assert!(!region.contains(position), "{position:?}");
    }
}

#[test]
fn regions_reject_nonfinite_positions_and_empty_areas_without_overflow() {
    let region = Region {
        x: u32::MAX,
        y: u32::MAX,
        width: 100,
        height: 100,
        scale: 1.0,
    };
    let origin = f64::from(u32::MAX);
    assert!(region.contains(Position::new(origin + 99.0, origin + 99.0)));
    assert!(!region.contains(Position::new(origin + 100.0, origin)));
    for value in [f64::NAN, f64::NEG_INFINITY, f64::INFINITY] {
        assert!(!region.contains(Position::new(value, origin)));
        assert!(!region.contains(Position::new(origin, value)));
    }
    for (width, height) in [(0, 100), (100, 0), (0, 0)] {
        let empty = Region {
            width,
            height,
            ..region
        };
        assert!(!empty.contains(Position::new(origin, origin)));
    }
}
