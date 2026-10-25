//! The first-run chooser and writer: [`choose`] as a pure function over a list of
//! candidates, and [`ensure_config`] / [`set_listen_address`] against a temporary
//! `--root`-style directory.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use config::first_run::{
    choose, ensure_config, lan_addresses, ranked_addresses, set_listen_address, Candidate,
};
use config::{Config, Paths};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private directory under the system temp root; the tests never touch the
/// per-user configuration directories.
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lws-first-run-tests-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temporary directory must be creatable");
    dir
}

/// The `Paths` of a fresh `--root` tree, exactly the shape `lws-core` builds.
fn paths_under(root: &Path) -> Paths {
    Paths::new(
        root.join("config"),
        root.join("data"),
        root.join("state"),
        Some(root.join("run")),
    )
}

/// One candidate; `is_up`/`has_default_route` are the only properties the chooser
/// reads besides the name and the address.
fn candidate(name: &str, address: Ipv4Addr, is_up: bool, has_default_route: bool) -> Candidate {
    Candidate {
        name: name.to_owned(),
        address,
        index: 1,
        prefix_len: 24,
        is_up,
        has_default_route,
    }
}

#[test]
fn a_windows_like_list_prefers_the_physical_adapter() {
    // What a Windows host with Hyper-V and VirtualBox installed actually reports:
    // three up adapters, two of them virtual, and the route through the physical
    // one. The virtual names are filtered even where they hold the route.
    let candidates = vec![
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, true),
        candidate(
            "vEthernet (Default Switch)",
            Ipv4Addr::new(172, 20, 96, 1),
            true,
            true,
        ),
        candidate(
            "VirtualBox Host-Only Network",
            Ipv4Addr::new(192, 168, 56, 1),
            true,
            false,
        ),
    ];

    assert_eq!(choose(&candidates), Some(Ipv4Addr::new(192, 168, 1, 10)));
}

#[test]
fn no_candidate_chooses_nothing() {
    assert_eq!(choose(&[]), None);
}

#[test]
fn loopback_and_link_local_and_multicast_are_not_lan_addresses() {
    let only_loopback = [candidate("Ethernet", Ipv4Addr::LOCALHOST, true, true)];
    assert_eq!(choose(&only_loopback), None);

    let only_link_local = [candidate(
        "wlan0",
        Ipv4Addr::new(169, 254, 7, 3),
        true,
        true,
    )];
    assert_eq!(choose(&only_link_local), None);

    let mcast = [
        candidate("wlan0", Ipv4Addr::new(224, 0, 0, 251), true, true),
        candidate("Ethernet", Ipv4Addr::UNSPECIFIED, true, true),
    ];
    assert_eq!(choose(&mcast), None);

    // The host name filter never brings them back either.
    let named = [
        candidate("lo", Ipv4Addr::LOCALHOST, true, true),
        candidate("Ethernet", Ipv4Addr::new(127, 0, 0, 1), true, true),
    ];
    assert_eq!(choose(&named), None);
}

#[test]
fn every_documented_virtual_adapter_name_is_filtered() {
    // One physical candidate, so each virtual name is what the choice turns on.
    let physical = || candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, false);
    let noise = |name: &str| candidate(name, Ipv4Addr::new(10, 7, 0, 2), true, true);
    for name in [
        "vEthernet (WSL)",
        "Hyper-V Virtual Ethernet Adapter",
        "vEthernet (Default Switch)",
        "docker0",
        "br-1a2b3c4d5e6f",
        "veth9f0d21",
        "virbr0",
        "WSL",
        "VirtualBox Host-Only Network",
        "vboxnet0",
        "VMware Network Adapter VMnet8",
        "vmnet1",
        "Loopback Pseudo-Interface 1",
        "lo",
        "Bluetooth Network Connection",
        "Npcap Loopback Adapter",
        "Radmin VPN",
    ] {
        assert_eq!(
            choose(&[noise(name), physical()]),
            Some(Ipv4Addr::new(192, 168, 1, 10)),
            "a peer must never be dialled through {name:?}"
        );
        assert!(
            ranked_addresses(&[noise(name)]).is_empty(),
            "{name:?} is not an address another computer can dial"
        );
    }
    // The same fragments are matched case-insensitively against the platform's
    // own spelling.
    assert_eq!(
        choose(&[candidate(
            "VETHERNET (External Switch)",
            Ipv4Addr::new(172, 20, 96, 1),
            true,
            true
        )]),
        None
    );
}

#[test]
fn vpn_and_overlay_interfaces_are_offered_below_the_physical_lan() {
    // A laptop on a cable and on Tailscale: the settings screen must list both — people do
    // run Bifrost over a mesh — but the default address stays the cable's, even when the
    // mesh holds the default route.
    let tailscale = Ipv4Addr::new(100, 100, 44, 2);
    let lan = Ipv4Addr::new(192, 168, 1, 10);
    let candidates = [
        candidate("tailscale0", tailscale, true, true),
        candidate("Ethernet", lan, true, false),
    ];
    assert_eq!(choose(&candidates), Some(lan));
    assert_eq!(ranked_addresses(&candidates), vec![lan, tailscale]);

    // The same, for every overlay spelling the platform uses.
    for name in [
        "tailscale0",
        "Tailscale",
        "wg0",
        "wireguard",
        "WireGuard Tunnel",
        "tun0",
        "tap0",
        "utun3",
        "ZeroTier One [a1b2c3d4e5]",
        "ztabcdef12345678",
    ] {
        let overlay = candidate(name, Ipv4Addr::new(10, 8, 0, 2), true, true);
        assert_eq!(
            choose(&[overlay.clone(), candidate("Ethernet", lan, true, false)]),
            Some(lan),
            "{name:?} must not take the default from the LAN"
        );
        assert_eq!(
            ranked_addresses(std::slice::from_ref(&overlay)),
            vec![Ipv4Addr::new(10, 8, 0, 2)],
            "{name:?} must stay selectable in the settings screen"
        );
    }
}

#[test]
fn a_friendly_name_that_merely_contains_a_tunnel_word_is_not_an_overlay() {
    // The old `contains` rule matched `tun` inside `Tuning` and `wg` at the end of a name,
    // and quietly dropped a working physical adapter from the list.
    for name in [
        "Ethernet (Tuning)",
        "Office-tuning",
        "enp3s0_wg",
        "Tunnelblick Ethernet",
    ] {
        let physical = candidate(name, Ipv4Addr::new(192, 168, 1, 10), true, true);
        assert_eq!(
            ranked_addresses(&[physical]),
            vec![Ipv4Addr::new(192, 168, 1, 10)],
            "{name:?} is a physical adapter"
        );
    }
}

#[test]
fn a_vpn_holding_the_default_route_does_not_win() {
    // The route the kernel would use goes through the tunnel; the tunnel is still
    // the wrong interface for this node to listen on.
    let candidates = [
        candidate("wg0", Ipv4Addr::new(10, 8, 0, 2), true, true),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, false),
    ];
    assert_eq!(choose(&candidates), Some(Ipv4Addr::new(192, 168, 1, 10)));

    // A tunnel the filter cannot recognise (a name without an overlay token) gets no
    // special treatment: on merit it wins, which is exactly why the name rule is part of
    // the ranking and not an optimisation.
    let plain = [
        candidate("eth0", Ipv4Addr::new(10, 0, 0, 2), true, true),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, true),
    ];
    assert_eq!(choose(&plain), Some(Ipv4Addr::new(10, 0, 0, 2)));
}

#[test]
fn the_default_route_outranks_an_up_interface() {
    let candidates = [
        candidate("wlan0", Ipv4Addr::new(192, 168, 1, 20), true, false),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), false, true),
    ];
    assert_eq!(choose(&candidates), Some(Ipv4Addr::new(192, 168, 1, 10)));
}

#[test]
fn up_outranks_a_quiet_interface_and_the_platform_order_decides() {
    // An up interface beats a quiet one, whatever order the platform listed them in.
    let up_later = [
        candidate("wlan0", Ipv4Addr::new(192, 168, 1, 20), false, false),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, false),
    ];
    assert_eq!(choose(&up_later), Some(Ipv4Addr::new(192, 168, 1, 10)));

    // Neither property, and neither filtered: the first the platform offered.
    let quiet = [
        candidate("wlan0", Ipv4Addr::new(192, 168, 1, 20), false, false),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), false, false),
    ];
    assert_eq!(choose(&quiet), Some(Ipv4Addr::new(192, 168, 1, 20)));

    // Equal rank: the platform's own order, not the smaller address.
    let equal = [
        candidate("wlan0", Ipv4Addr::new(192, 168, 1, 20), true, true),
        candidate("Ethernet", Ipv4Addr::new(192, 168, 1, 10), true, true),
    ];
    assert_eq!(choose(&equal), Some(Ipv4Addr::new(192, 168, 1, 20)));
}

#[test]
fn benchmarking_network_is_not_a_lan_candidate() {
    let benchmarking = candidate("FlClashX", Ipv4Addr::new(198, 18, 0, 1), true, true);
    let ethernet = candidate("Ethernet 5", Ipv4Addr::new(192, 168, 31, 37), true, false);
    assert_eq!(
        choose(&[benchmarking, ethernet]),
        Some(Ipv4Addr::new(192, 168, 31, 37))
    );
}

#[test]
fn lan_addresses_offers_only_addresses_a_peer_could_dial() {
    // The platform half is real here; the assertion is about the filter and the
    // order, never about how many addresses this host happens to have.
    let found = lan_addresses();
    for address in &found {
        assert!(
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_link_local()
                && !address.is_multicast(),
            "{address} is not an address another node could dial"
        );
    }
    // The same address is never offered twice, even when two adapters carry it.
    let mut sorted = found.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), found.len());
}

#[test]
fn ensure_config_writes_a_valid_document_and_leaves_one_alone() {
    let root = temp_dir("ensure");
    let paths = paths_under(&root);

    assert_eq!(ensure_config(&paths), Ok(true));
    let file = paths.config_file();
    assert!(file.exists(), "config.toml must appear");

    let text = std::fs::read_to_string(&file).expect("the written configuration is readable");
    let config = Config::from_str(&text).expect("the written text parses");
    config.validate().expect("the written document validates");

    assert_eq!(config.config_version, 2);
    assert!(!config.node.display_name.trim().is_empty());
    assert!(!config.network.listen_address.is_empty());
    assert!(config.node.can_own);
    assert_eq!(config.node.claim_on_startup, cfg!(windows));
    // ADR-008: the channel is opt-in, so the first run leaves it off.
    assert!(!config.clipboard.enabled);

    // The written address is what this host's own chooser picked — and when it
    // picked nothing, the documented fallback pair the validation table allows.
    match lan_addresses().first().copied() {
        Some(best) => assert_eq!(config.network.listen_address, best.to_string()),
        None => {
            assert_eq!(config.network.listen_address, "0.0.0.0");
            assert!(config.network.allow_any_interface);
        }
    }

    // A second call changes nothing at all.
    let before = std::fs::read_to_string(&file).expect("still readable");
    assert_eq!(ensure_config(&paths), Ok(false));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.config_dir())
            .expect("the configuration directory exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the directory is owner-only (0700)");
    }
}

#[test]
fn set_listen_address_replaces_the_field_and_saves_atomically() {
    let root = temp_dir("set");
    let paths = paths_under(&root);
    ensure_config(&paths).expect("the first-run configuration is written");

    // A documentation address (RFC 5737): the test never binds what it writes.
    let address = Ipv4Addr::new(192, 0, 2, 200);
    set_listen_address(&paths, address).expect("the address is saved");
    let config = Config::load(paths.config_file()).expect("the saved document loads");
    assert_eq!(config.network.listen_address, address.to_string());
    config.validate().expect("it still validates");

    // An unspecified address keeps the flag the validation table demands, which is
    // why the pair stays loadable.
    set_listen_address(&paths, Ipv4Addr::UNSPECIFIED).expect("the address is saved");
    let config = Config::load(paths.config_file()).expect("the saved document loads");
    assert_eq!(config.network.listen_address, "0.0.0.0");
    assert!(config.network.allow_any_interface);
    assert!(!config.node.display_name.is_empty(), "the rest survives");
}

#[test]
fn set_listen_address_never_invents_a_configuration() {
    let root = temp_dir("set-missing");
    let paths = paths_under(&root);
    assert!(
        set_listen_address(&paths, Ipv4Addr::new(192, 168, 1, 10)).is_err(),
        "only ensure_config creates a file"
    );
    assert!(!paths.config_file().exists());
}
