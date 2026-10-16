//! Agent rule 1 (`docs/03-architecture.md`): the agent owns no network sockets.
//!
//! The agent used to be its own executable, and the Windows build proved the rule at link
//! time. The one `bifrost` executable links Winsock for the core, so the proof is now
//! made here, on every `cargo test --workspace` (and by the `agent-no-network` CI job):
//!
//! * the agent crate's normal dependency tree holds none of the crates that exist to talk to
//!   a network;
//! * no source file of the agent names a TCP/UDP socket type;
//! * the run-time guard (`lws_agent::netguard`) really sees a TCP or UDP socket of this
//!   process when there is one, and sees none when there is not.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Crates that must never be reachable from `lws-agent`: the link/TLS/identity stack of the
/// core, the window, and the usual network clients.
const DENIED_CRATES: &[&str] = &[
    "transport",
    "identity",
    "focus-router",
    "lws-core",
    "lws-gui",
    "bifrost",
    "eframe",
    "rustls",
    "tokio-rustls",
    "rustls-webpki",
    "native-tls",
    "openssl",
    "hyper",
    "h2",
    "reqwest",
    "ureq",
    "quinn",
    "mdns-sd",
];

/// Source-level markers of a network socket. The agent's local IPC is a named pipe / a unix
/// socket (`ipc` crate), and none of these is needed for it.
const DENIED_SOURCE: &[&str] = &[
    "std::net",
    "tokio::net::Tcp",
    "tokio::net::Udp",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "socket2",
    "ToSocketAddrs",
    "WSAStartup",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn the_dependency_tree_has_no_network_crates() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(manifest_dir())
        .args([
            "tree",
            "-p",
            "lws-agent",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--locked",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    let names: Vec<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(
        names.contains(&"lws-agent") && names.contains(&"ipc"),
        "the tree is not the agent's:\n{tree}"
    );
    let found: Vec<&&str> = names
        .iter()
        .filter(|name| DENIED_CRATES.contains(*name))
        .collect();
    assert!(
        found.is_empty(),
        "lws-agent must not depend on network crates (rule 1), but its tree has {found:?}"
    );
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source directory").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_source_file_names_a_network_socket() {
    let mut files = Vec::new();
    rust_files(&manifest_dir().join("src"), &mut files);
    assert!(files.len() > 5, "the agent's sources were not found");
    let mut hits = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("source file");
        for (index, line) in text.lines().enumerate() {
            for marker in DENIED_SOURCE {
                if line.contains(marker) {
                    hits.push(format!("{}:{}: {marker}", file.display(), index + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "network socket types in the agent (rule 1):\n{}",
        hits.join("\n")
    );
}

/// One test, in order: this process is the only one whose sockets are inspected, and other
/// tests of this binary must not open sockets while it runs.
#[test]
fn the_guard_sees_exactly_the_sockets_this_process_owns() {
    assert_eq!(
        lws_agent::netguard::find_network_socket().expect("the guard can inspect this process"),
        None,
        "a fresh test process owns no network socket"
    );

    let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("a UDP socket");
    let found = lws_agent::netguard::find_network_socket().expect("the guard can inspect");
    assert!(found.is_some(), "an open UDP socket must be reported");
    drop(udp);

    let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("a TCP listener");
    let found = lws_agent::netguard::find_network_socket().expect("the guard can inspect");
    assert!(found.is_some(), "an open TCP listener must be reported");
    drop(tcp);

    assert_eq!(
        lws_agent::netguard::find_network_socket().expect("the guard can inspect"),
        None,
        "closed sockets are not reported"
    );
}
