//! First run: the configuration file exists before anything reads it.
//!
//! `lws-core run` used to die on a missing `config.toml`, and the owner of the
//! machine is not a developer — AC-16 («настройка пары без CLI ≤3 мин») leaves no
//! room for editing TOML by hand. So the first `run` writes a valid file itself,
//! from the schema defaults plus what this host can say about its own network,
//! and `lws-gui` edits one field of it (`set_listen_address`) from its settings
//! screen.
//!
//! What the first run writes — every value documented in
//! `docs/config.example.toml` / `crates/config/examples-data/config.example.toml`:
//!
//! | key | value |
//! |---|---|
//! | `config_version` | [`SUPPORTED_CONFIG_VERSION`](crate::SUPPORTED_CONFIG_VERSION) |
//! | `node.display_name` | the hostname, trimmed |
//! | `node.can_own` | `true` |
//! | `node.claim_on_startup` | `true` on Windows, `false` elsewhere — the example's own comment, «Windows: true, ноутбуки: false» |
//! | `network.listen_address` | the first of [`lan_addresses`]; see below when that is empty |
//! | everything else | the schema defaults ([`Config::default`]), the clipboard off with them (ADR-008: the channel is opt-in) |
//!
//! The empty-list fallback is written down here and in
//! `docs/03-architecture.md`, «Валидация при загрузке»: a concrete private LAN
//! address is the rule, and «`0.0.0.0` только с явным `allow_any_interface =
//! true`» is the named exception — which [`NetworkConfig::validate`] enforces by
//! refusing an unspecified address without the flag. A first run with no
//! discoverable interface still has to be pairable, so that combination is the
//! fallback, with a warning on stderr: the listener binds every interface, a peer
//! reaches it no matter which one it is on, and the settings screen then pins a
//! concrete address. `127.0.0.1` was rejected as the fallback because it would
//! put every peer on the LAN out of reach, which is the one thing the first run
//! must not do.
//!
//! The chooser is pure ([`choose`], [`Candidate`]); only the enumeration knows a
//! platform. `lws-gui` calls [`lan_addresses`] and [`set_listen_address`];
//! `lws-core` calls [`ensure_config`].

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

use crate::schema::{Config, NetworkConfig, NodeConfig};
use crate::{save_atomic, Paths};

/// One address the platform offered, with what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The adapter name as the platform reports it: `Ethernet`, `wlan0`,
    /// `vEthernet (Default Switch)`, `VirtualBox Host-Only Network`.
    pub name: String,
    /// The IPv4 address that adapter carries.
    pub address: Ipv4Addr,
    /// OS interface index for multicast membership and outgoing-interface selection.
    pub index: u32,
    /// IPv4 on-link subnet prefix length, used to reject off-LAN discovery hints.
    pub prefix_len: u8,
    /// The address the kernel would use to reach the internet, marked by the route
    /// probe ([`default_route_address`]) after the enumeration.
    pub has_default_route: bool,
    /// The interface reports itself up: `OperStatus = up` on Windows, `IFF_UP` on
    /// Unix.
    pub is_up: bool,
}

/// Adapter-name fragments that are never the LAN this node listens on.
///
/// Matched case-insensitively with `contains` against the platform's own name for
/// the interface, which is how each of them shows up in practice:
/// `vEthernet (Default Switch)` and other Hyper-V virtual adapters, `docker0`,
/// `WSL` (`vEthernet (WSL (Hyper-V firewall))`), `VirtualBox Host-Only Network` /
/// `vboxnet0`, `VMware Network Adapter VMnet8` / `vmnet1`, `loopback` /
/// the `lo`-named interface, `Bluetooth Network Connection`, the Npcap loopback and
/// the Radmin adapters. A physical `Ethernet`, `enp3s0` or `wlan0` never contains one
/// of these fragments.
///
/// VPN and overlay interfaces are **not** on this list: people do run Bifrost over
/// WireGuard, Tailscale and ZeroTier, so those are offered and remain selectable in the
/// settings screen. They are only *ranked* below the physical LAN ([`is_overlay`]), which
/// keeps the default address on the cable.
///
/// The match is a *name* rule and nothing more: an adapter whose name carries none
/// of these fragments — a proxy client's own tunnel, say — is offered by
/// [`lan_addresses`] like any other, which is why the GUI pins the address it
/// wants through [`set_listen_address`] instead of trusting the head of the list.
const VIRTUAL_ADAPTERS: &[&str] = &[
    "vethernet",
    "hyper-v",
    "docker",
    "wsl",
    "virtualbox",
    "vbox",
    "vmware",
    "vmnet",
    "loopback",
    "bluetooth",
    "npcap",
    "radmin",
];

/// Adapter-name prefixes of the bridges and veth pairs Linux creates: `br-1a2b3c`,
/// `veth9f0d`, `virbr0`. Matched as prefixes because the random suffix is what follows.
const VIRTUAL_PREFIXES: &[&str] = &["br-", "veth", "virbr"];

/// Adapter-name prefixes of VPN and overlay interfaces, matched as tokens (see
/// [`starts_with_token`]): WireGuard (`wg0`), OpenVPN (`tun0`, `tap0`), macOS/UTUN
/// (`utun3`), Tailscale (`tailscale0`, the FriendlyName `Tailscale`), ZeroTier (`zt…`
/// and `ZeroTier One [<node id>]`).
///
/// These are kept — a mesh is a working transport for Bifrost — but [`rank`] puts every
/// physical interface above them, so a laptop with both an Ethernet cable and Tailscale
/// still listens on the LAN by default.
const OVERLAY_PREFIXES: &[&str] = &[
    "tailscale",
    "wireguard",
    "zerotier",
    "wg",
    "utun",
    "tun",
    "tap",
    "zt",
];

/// The address the route probe "connects" to: `192.0.2.1` is TEST-NET-1
/// (RFC 5737), reserved for documentation and never routed, and port 9 is
/// discard. Nothing leaves the machine either way.
const PROBE_ADDRESS: &str = "192.0.2.1:9";

/// The machine's LAN IPv4 addresses, best candidate first.
///
/// Loopback, unspecified, link-local, multicast and virtual-adapter addresses are
/// left out by the same rules [`choose`] uses; VPN and overlay interfaces stay, below the
/// physical ones. When nothing qualifies the vector is empty and the caller's own fallback
/// applies.
pub fn lan_addresses() -> Vec<Ipv4Addr> {
    let mut offered = platform_candidates();
    mark_default_route(&mut offered, default_route_address());
    ranked_addresses(&offered)
}

/// Eligible, up LAN IPv4 interfaces in the same preference order as [`lan_addresses`].
///
/// Preserves each address, OS interface index and adapter name for multicast
/// discovery. No extra enumeration dependency is needed; unavailable indices and
/// down interfaces cannot join a group and are omitted.
pub fn lan_interfaces() -> Vec<Candidate> {
    let mut offered = platform_candidates();
    mark_default_route(&mut offered, default_route_address());
    offered.retain(|candidate| {
        candidate.is_up
            && candidate.index != 0
            && candidate.prefix_len <= 32
            && qualifies(&candidate)
    });
    offered.sort_by_key(|candidate| core::cmp::Reverse(rank(candidate)));
    offered
}

/// Discovery interfaces restricted to the configured input/control listener.
///
/// An explicit IPv4 address selects only that address; an unspecified listener
/// selects all eligible LAN interfaces only when `allow_any_interface` permits it.
/// IPv6 listeners do not participate in IPv4 multicast discovery.
pub fn discoverable_interfaces(network: &NetworkConfig) -> Vec<Candidate> {
    let Ok(listen) = network.listen_address.parse::<Ipv4Addr>() else {
        return Vec::new();
    };
    if listen.is_unspecified() && !network.allow_any_interface {
        return Vec::new();
    }
    let mut interfaces = lan_interfaces();
    if !listen.is_unspecified() {
        interfaces.retain(|candidate| candidate.address == listen);
    } else {
        interfaces.retain(|candidate| !is_overlay(&candidate.name));
    }
    interfaces
}

/// [`lan_addresses`]'s pure half: `candidates` filtered and ordered, best first.
///
/// The same list the settings screen offers, so a test can assert the order — and that an
/// overlay is still on it — without a network.
pub fn ranked_addresses(candidates: &[Candidate]) -> Vec<Ipv4Addr> {
    let mut ranked = ranked(candidates);
    // The same address on two adapters (a reconnected VPN, a stale tunnel) is one
    // address; the better position of the two wins.
    ranked.dedup_by(|first, second| first.address == second.address);
    ranked
        .into_iter()
        .map(|candidate| candidate.address)
        .collect()
}

/// The best LAN address of `candidates`, or `None` when none qualifies.
///
/// Pure — no platform calls, no clock, no I/O — so the rules are testable without
/// a network. The filters, in order:
///
/// 1. drop loopback (`127.0.0.0/8`), unspecified (`0.0.0.0`), link-local
///    (`169.254.0.0/16`), multicast, and RFC 2544 benchmarking (`198.18.0.0/15`)
///    addresses: they are not addresses another node on the LAN could dial;
/// 2. drop candidates whose name contains a virtual-adapter fragment
///    ([`VIRTUAL_ADAPTERS`]) or starts with a bridge/veth prefix: a Hyper-V switch or a
///    Docker bridge is not an address another computer reaches this one on;
/// 3. what is left ranks a physical interface above a VPN/overlay one ([`is_overlay`]),
///    and inside those two classes `has_default_route` above `is_up` — the address the
///    kernel already uses for the default route is the one the peers can reach — and the
///    first of the highest rank wins.
pub fn choose(candidates: &[Candidate]) -> Option<Ipv4Addr> {
    ranked(candidates)
        .first()
        .map(|candidate| candidate.address)
}

/// Writes a valid default `config.toml` when there is none.
///
/// `Ok(true)` means it created one; `Ok(false)` means the file was already there
/// and was left untouched. The written document passes [`Config::validate`]:
/// `config_version = 2`, a non-empty `display_name`, a non-empty
/// `listen_address`, `can_own = true`, `claim_on_startup` by host (see the module
/// docs) and the schema defaults everywhere else.
pub fn ensure_config(paths: &Paths) -> Result<bool, String> {
    let path = paths.config_file();
    if path.exists() {
        return Ok(false);
    }
    let listen = lan_addresses().into_iter().next();
    if listen.is_none() {
        // The written pair (see the module docs) is the one validation accepts, and
        // a person has to be able to see why the file does not name an interface.
        eprintln!(
            "warning: no LAN address found on this host; writing config.toml with \
             listen_address = 0.0.0.0 and allow_any_interface = true — \
             pick a concrete address in the settings screen"
        );
    }
    // The directory first (the lock file lives in it, and the permission policy is this
    // function's job), then the lock: two first runs (two CLIs, a CLI racing a core's first
    // persistence) must not both decide the file is missing and write their own copy over the
    // other's (audit F17). The existence check is repeated inside the lock for that reason.
    create_config_dir(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    let _lock = crate::lock::FileLock::acquire(&path).map_err(|error| error.to_string())?;
    if path.exists() {
        return Ok(false);
    }
    let config = default_config(listen);
    let text = config.to_toml().map_err(|error| error.to_string())?;
    save_atomic(&path, &text)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(true)
}

/// Replaces `[network] listen_address` in the existing configuration and saves it
/// atomically.
///
/// A missing or damaged configuration is an error, not a silent reset — only
/// [`ensure_config`] creates one.
pub fn set_listen_address(paths: &Paths, address: Ipv4Addr) -> Result<(), String> {
    let path = paths.config_file();
    // The whole read-modify-write, under the same lock every other writer takes (audit F17).
    let _lock = crate::lock::FileLock::acquire(&path).map_err(|error| error.to_string())?;
    let mut config = Config::load(&path).map_err(|error| error.to_string())?;
    config.network.listen_address = address.to_string();
    if address.is_unspecified() {
        // `NetworkConfig::validate` refuses an unspecified address without the
        // explicit flag («только с явным `allow_any_interface = true`»); a flag the
        // file already carries is never taken away here.
        config.network.allow_any_interface = true;
    }
    let text = config.to_toml().map_err(|error| error.to_string())?;
    save_atomic(&path, &text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// `candidates` after the filters, ordered best-first.
///
/// The single source of the preference order, shared by [`choose`] and
/// [`lan_addresses`] so the head of the list is always what [`choose`] picks.
fn ranked(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut kept: Vec<&Candidate> = candidates.iter().filter(qualifies).collect();
    // A stable sort, so equal ranks keep the platform's own order.
    kept.sort_by_key(|candidate| core::cmp::Reverse(rank(candidate)));
    kept
}

/// The filters of [`choose`], as one predicate.
fn qualifies(candidate: &&Candidate) -> bool {
    let address = candidate.address;
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_link_local()
        && !address.is_multicast()
        && !is_benchmarking(address)
        && !is_virtual(&candidate.name)
}

/// RFC 2544 reserves 198.18.0.0/15 for benchmarking, not peer-reachable LAN use.
fn is_benchmarking(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 198 && (18..=19).contains(&octets[1])
}

/// The preference score of one qualifying candidate: higher is better.
///
/// A physical interface outranks every overlay, however the overlay is routed: a laptop
/// with a cable and a WireGuard tunnel listens on the cable by default, and the tunnel
/// address stays offered one position down. Inside each class `has_default_route`
/// outranks `is_up`: an up interface the kernel refuses to route through is still
/// unreachable, and a route the platform cannot see as up (a tunnel mid-handshake) still
/// carries the traffic. Equal scores are left to the platform's own order by the stable
/// sort in [`ranked`].
fn rank(candidate: &Candidate) -> u8 {
    let mut score = if is_overlay(&candidate.name) {
        0
    } else {
        0b100
    };
    if candidate.has_default_route {
        score |= 0b10;
    }
    if candidate.is_up {
        score |= 0b01;
    }
    score
}

/// Whether `name` is an interface nothing outside this machine can reach through:
/// loopback, a Hyper-V/WSL/VirtualBox/VMware switch, or a Linux bridge/veth pair.
fn is_virtual(name: &str) -> bool {
    let lowered = name.trim().to_lowercase();
    lowered == "lo"
        || VIRTUAL_ADAPTERS
            .iter()
            .any(|fragment| lowered.contains(*fragment))
        || VIRTUAL_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(*prefix))
}

/// Whether `name` is a VPN or overlay interface: kept, but ranked below physical ones.
fn is_overlay(name: &str) -> bool {
    let lowered = name.trim().to_lowercase();
    OVERLAY_PREFIXES
        .iter()
        .any(|prefix| starts_with_token(&lowered, prefix))
}

/// Whether `lowered` starts with `token` as a whole word.
///
/// The substring rule this replaces matched `tun` inside `Ethernet (Tuning)` and `wg`
/// inside `enp3s0_wg`. A token is the word itself (`Tailscale`), the word and a number
/// (`wg0`, `tun0`, `utun3`, `tailscale0`), ZeroTier's `zt` followed by its hex network id,
/// or the word and a separator (`WireGuard Tunnel`, `ZeroTier One [<node id>]`). Anything
/// else is a different word: `tuning` is not `tun`.
fn starts_with_token(lowered: &str, token: &str) -> bool {
    let Some(rest) = lowered.strip_prefix(token) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(c) if c.is_ascii_digit() => true,
        Some(c) if token == "zt" && c.is_ascii_hexdigit() => true,
        Some(c) => !c.is_alphanumeric(),
    }
}

/// The first-run document: the documented ownership defaults and the schema
/// defaults everywhere else.
fn default_config(listen: Option<Ipv4Addr>) -> Config {
    let (listen_address, allow_any_interface) = match listen {
        Some(address) => (address.to_string(), false),
        None => (
            Ipv4Addr::UNSPECIFIED.to_string(),
            // The one combination the validation table names for an unspecified
            // address; the fallback rule is written down in the module docs.
            true,
        ),
    };
    Config {
        config_version: crate::SUPPORTED_CONFIG_VERSION,
        node: NodeConfig {
            display_name: hostname(),
            can_own: true,
            // «Windows: true, ноутбуки: false» — the example's own comment. A Windows
            // desktop is the machine the user is sitting at; a laptop joining somebody
            // else's workspace must not claim ownership of it on every start.
            claim_on_startup: cfg!(windows),
        },
        network: NetworkConfig {
            listen_address,
            allow_any_interface,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The hostname, trimmed; the schema's own default when the platform will not
/// say — an empty `display_name` is a useless label in the GUI.
fn hostname() -> String {
    platform_hostname().unwrap_or_else(|| NodeConfig::default().display_name)
}

/// The hostname from the operating system, when it has one.
#[cfg(unix)]
fn platform_hostname() -> Option<String> {
    // The kernel's own name first: `$HOSTNAME` is a shell variable and is absent
    // from a systemd service environment.
    let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let trimmed = kernel.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_owned());
    }
    std::env::var("HOSTNAME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// The hostname from the environment: Windows always exports `COMPUTERNAME` to
/// every process, which is the same string `GetComputerNameExW` would return for
/// the `ComputerNameDnsHostname` flavour the GUI shows next to an address.
#[cfg(windows)]
fn platform_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Neither documented platform: no hostname, so the schema's own default label.
#[cfg(not(any(windows, unix)))]
fn platform_hostname() -> Option<String> {
    None
}

/// The directory that will hold `config.toml`, owner-only.
///
/// `docs/03-architecture.md`, «Файлы и пути»: «Каталоги создаются с правами
/// только для пользователя: ACL на текущий SID в Windows, 0700 в Linux». On Unix
/// this crate sets `0700` itself — it owns the documented paths and makes no
/// platform calls of its own. On Windows the owner-only ACL is `identity`'s
/// business (`identity::acl`, for keys; `identity::atomic::create_private_dir`,
/// which `lws-core` runs over these very directories before it reads the
/// configuration), so this only creates the directory — the same thing
/// [`save_atomic`] would do, made explicit so the permission does not depend on
/// the caller's umask.
#[cfg(unix)]
fn create_config_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let directory = config_dir_of(path);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
}

/// Windows: create the directory, ACL left to the identity layer (see the Unix
/// twin and [`save_atomic`]).
#[cfg(windows)]
fn create_config_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir_of(path))
}

#[cfg(not(any(windows, unix)))]
fn create_config_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir_of(path))
}

/// The directory of `config.toml`, never empty (a bare file name means "here").
fn config_dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// What this host's OS says about its interfaces; empty when the call fails.
///
/// `has_default_route` is left `false` here: the route probe is the same on every
/// platform and marks it in [`mark_default_route`].
#[cfg(windows)]
fn platform_candidates() -> Vec<Candidate> {
    win32::candidates()
}

#[cfg(unix)]
fn platform_candidates() -> Vec<Candidate> {
    posix::candidates()
}

/// Neither documented platform: no interface enumeration, which is an empty list,
/// which is the documented fallback — never a panic.
#[cfg(not(any(windows, unix)))]
fn platform_candidates() -> Vec<Candidate> {
    Vec::new()
}

/// The address the kernel would use for the default route — without a packet
/// leaving the machine.
///
/// A connected UDP socket transmits nothing until it is written to, so binding
/// `0.0.0.0:0` and connecting to a documentation address asks the routing table
/// which source it would pick. `None` when there is no route at all, which is the
/// host that gets the fallback.
fn default_route_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(PROBE_ADDRESS).ok()?;
    match socket.local_addr() {
        Ok(SocketAddr::V4(address)) => Some(*address.ip()),
        // A v6-only route (or nothing at all) leaves this node with no candidate
        // the chooser can use.
        _ => None,
    }
}

/// `has_default_route` on whichever candidate holds the probe's answer.
///
/// The probe can name an address the platform list does not have — a route through
/// a tunnel the enumeration skipped — and an address without a name cannot be
/// filtered for virtual adapters, so none is invented here.
fn mark_default_route(candidates: &mut [Candidate], probe: Option<Ipv4Addr>) {
    let Some(probe) = probe else {
        return;
    };
    for candidate in candidates.iter_mut() {
        if candidate.address == probe {
            candidate.has_default_route = true;
        }
    }
}

/// Win32 interface enumeration for [`platform_candidates`].
///
/// `GetAdaptersAddresses` is the only source that carries both halves the chooser
/// needs: the friendly name, which is what makes the virtual-adapter filter
/// possible, and `OperStatus`, the `is_up` of the tie-break.
#[cfg(windows)]
#[allow(unsafe_code)]
mod win32 {
    use std::mem::size_of;
    use std::net::Ipv4Addr;

    use super::Candidate;
    use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows_sys::Win32::NetworkManagement::IpHelper;
    use windows_sys::Win32::NetworkManagement::Ndis;
    use windows_sys::Win32::Networking::WinSock;

    /// Every unicast IPv4 address this host's adapters carry.
    ///
    /// Anycast, multicast and DNS entries are skipped with the flags; link-local
    /// addresses are still offered here and dropped by the chooser, so no rule
    /// lives in two places.
    pub(super) fn candidates() -> Vec<Candidate> {
        let Some(adapters) = adapters() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        // SAFETY: `adapters.ptr` is the linked list the API built inside the memory
        // `adapters` owns, which stays alive for the whole walk because `adapters`
        // lives to the end of this function.
        unsafe {
            let mut adapter = adapters.ptr;
            while !adapter.is_null() {
                let name = wide_text((*adapter).FriendlyName);
                let is_up = (*adapter).OperStatus == Ndis::IfOperStatusUp;
                let mut address = (*adapter).FirstUnicastAddress;
                while !address.is_null() {
                    if let Some(ipv4) = sockaddr_ipv4((*address).Address) {
                        found.push(Candidate {
                            name: name.clone(),
                            address: ipv4,
                            index: (*adapter).Anonymous1.Anonymous.IfIndex,
                            prefix_len: (*address).OnLinkPrefixLength,
                            has_default_route: false,
                            is_up,
                        });
                    }
                    address = (*address).Next;
                }
                adapter = (*adapter).Next;
            }
        }
        found
    }

    /// The adapter list together with the memory it points into.
    struct Adapt {
        /// Kept alive so the linked list stays valid; the pointer into it is `ptr`.
        _words: Vec<u64>,
        ptr: *mut IpHelper::IP_ADAPTER_ADDRESSES_LH,
    }

    /// The two-call pattern `GetAdaptersAddresses` documents: measure, then fill.
    ///
    /// `None` means the call itself failed — the caller then has no addresses, which
    /// is the same answer as an empty list, and never a panic. An empty answer
    /// (no adapters at all) arrives as a null pointer, handled like a failure.
    fn adapters() -> Option<Adapt> {
        let flags = IpHelper::GAA_FLAG_SKIP_ANYCAST | IpHelper::GAA_FLAG_SKIP_MULTICAST;
        let mut size = 0u32;
        // SAFETY: the measuring call writes only `size`; the null buffer is what asks
        // for the size, and a null address pointer is what asks for that buffer.
        let measured = unsafe {
            IpHelper::GetAdaptersAddresses(
                WinSock::AF_INET as u32,
                flags,
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if measured == 0 {
            return Some(Adapt {
                _words: Vec::new(),
                ptr: std::ptr::null_mut(),
            });
        }
        if measured != ERROR_BUFFER_OVERFLOW {
            return None;
        }
        // The linked-list structs are 8-byte aligned, so the buffer is allocated in
        // `u64` words rather than bytes.
        let words = usize::try_from(size)
            .ok()?
            .div_ceil(size_of::<u64>())
            .max(1);
        let mut words = vec![0u64; words];
        let ptr = words
            .as_mut_ptr()
            .cast::<IpHelper::IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the buffer is at least `size` bytes and aligned for the structs the
        // API writes into it; the API writes nothing beyond the size it was given.
        let written = unsafe {
            IpHelper::GetAdaptersAddresses(
                WinSock::AF_INET as u32,
                flags,
                std::ptr::null(),
                ptr,
                &mut size,
            )
        };
        if written != 0 {
            return None;
        }
        Some(Adapt { _words: words, ptr })
    }

    /// The IPv4 address inside a `SOCKET_ADDRESS`, or `None` for anything else.
    fn sockaddr_ipv4(address: WinSock::SOCKET_ADDRESS) -> Option<Ipv4Addr> {
        let minimum = i32::try_from(size_of::<WinSock::SOCKADDR_IN>()).unwrap_or(i32::MAX);
        if address.lpSockaddr.is_null() || address.iSockaddrLength < minimum {
            return None;
        }
        // SAFETY: the API reports at least `SOCKADDR_IN` bytes at this pointer, and
        // the family is checked before the cast that reads the wider struct.
        unsafe {
            if (*address.lpSockaddr).sa_family != WinSock::AF_INET {
                return None;
            }
            let inet = &*address.lpSockaddr.cast::<WinSock::SOCKADDR_IN>();
            // `S_addr` holds the address in network byte order *in memory*, so the
            // register value is byte-swapped on a little-endian machine — and
            // `Ipv4Addr::from` reads the unswapped (first octet in the most
            // significant byte) integer.
            Some(Ipv4Addr::from(u32::from_be(inet.sin_addr.S_un.S_addr)))
        }
    }

    /// A `PWSTR` as a Rust `String`, for the adapter names the filter reads.
    fn wide_text(pointer: windows_sys::core::PWSTR) -> String {
        if pointer.is_null() {
            return String::new();
        }
        // SAFETY: `pointer` is a live NUL-terminated wide string — either the
        // adapter's `FriendlyName`, or the empty string.
        let length = unsafe { wide_length(pointer) };
        // SAFETY: `pointer` is a NUL-terminated wide string, so the `length` code
        // units before its terminator are initialized UTF-16.
        let units = unsafe { std::slice::from_raw_parts(pointer, length) };
        String::from_utf16_lossy(units)
    }

    /// The length of a NUL-terminated wide string, in code units.
    ///
    /// # Safety
    /// `pointer` must be a readable, NUL-terminated `*const u16`.
    unsafe fn wide_length(pointer: *const u16) -> usize {
        let mut length = 0;
        // SAFETY: the NUL terminator guaranteed by the caller stops the scan inside
        // the same allocation the string lives in.
        while unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        length
    }
}

/// Unix interface enumeration for [`platform_candidates`].
///
/// `getifaddrs` is the portable answer to the same two questions Win32 answers:
/// the interface name and the `IFF_UP` flag. Each entry of the list carries one
/// address, so there is no inner loop to walk.
#[cfg(unix)]
#[allow(unsafe_code)]
mod posix {
    use std::ffi::CStr;
    use std::net::Ipv4Addr;

    use super::Candidate;

    /// Every unicast IPv4 address this host's interfaces carry.
    ///
    /// Down interfaces are offered too, with `is_up = false`: the chooser decides.
    pub(super) fn candidates() -> Vec<Candidate> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` writes exactly this one pointer, and returns non-zero
        // (leaving it null) on any failure, which is the empty answer.
        if unsafe { libc::getifaddrs(&mut head) } != 0 || head.is_null() {
            return Vec::new();
        }
        let mut found = Vec::new();
        // SAFETY: the call above succeeded, so `head` starts a `Next`-terminated list
        // that nothing mutates and that `freeifaddrs`, at the end of this block,
        // releases as the list the API allocated.
        unsafe {
            let mut adapter = head;
            while !adapter.is_null() {
                // `ifa_flags` is `c_uint` on Linux and `c_int` on the BSDs, so the
                // comparison is made in one unsigned type.
                let is_up = ((*adapter).ifa_flags as u32 & libc::IFF_UP as u32) != 0;
                let name = name_of((*adapter).ifa_name);
                if let Some(ipv4) = sockaddr_ipv4((*adapter).ifa_addr) {
                    found.push(Candidate {
                        name,
                        address: ipv4,
                        prefix_len: sockaddr_ipv4((*adapter).ifa_netmask)
                            .and_then(|mask| {
                                let mask = u32::from(mask);
                                let prefix = mask.leading_ones();
                                (mask == u32::MAX.checked_shl(32 - prefix).unwrap_or(0))
                                    .then_some(prefix as u8)
                            })
                            .unwrap_or(33),
                        // SAFETY: the live `ifaddrs` entry owns a NUL-terminated name.
                        index: libc::if_nametoindex((*adapter).ifa_name),
                        has_default_route: false,
                        is_up,
                    });
                }
                adapter = (*adapter).ifa_next;
            }
            libc::freeifaddrs(head);
        }
        found
    }

    /// An interface name as a Rust `String` (`lo`, `eth0`, `enp3s0`).
    fn name_of(pointer: *mut libc::c_char) -> String {
        if pointer.is_null() {
            return String::new();
        }
        // SAFETY: `ifa_name` is a NUL-terminated string the list owns.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }

    /// The IPv4 address inside an `ifa_addr`, or `None` for anything else.
    fn sockaddr_ipv4(pointer: *mut libc::sockaddr) -> Option<Ipv4Addr> {
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the pointer comes from a live `ifaddrs` entry, and the family is
        // checked before the cast that reads the wider struct the entry points at.
        unsafe {
            if (*pointer).sa_family != libc::AF_INET as libc::sa_family_t {
                return None;
            }
            let inet = &*pointer.cast::<libc::sockaddr_in>();
            // `s_addr` holds the address in network byte order *in memory*, so the
            // register value is byte-swapped on a little-endian machine — and
            // `Ipv4Addr::from` reads the unswapped (first octet in the most
            // significant byte) integer.
            Some(Ipv4Addr::from(u32::from_be(inet.sin_addr.s_addr)))
        }
    }
}
