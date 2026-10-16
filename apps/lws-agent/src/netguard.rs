//! The agent's no-network rule, checked at run time.
//!
//! `docs/03-architecture.md`, agent rule 1: the agent owns no network sockets. While the
//! agent was its own executable the Windows build proved it at link time (no Winsock import
//! in the file). With one executable for every role that proof is gone — the binary links
//! Winsock for the core — so the rule is now held by three independent checks:
//!
//! 1. **Dependencies** (`tests/no_network.rs`, CI): this crate's `cargo tree` must not
//!    contain the transport, TLS or identity crates, nor any source line that names a TCP/UDP
//!    socket type.
//! 2. **This module, at start**: the process must own no TCP, UDP or raw socket — which also
//!    catches anything inherited from the parent.
//! 3. **This module, while running**: the same check every [`PERIOD`]. A violation is a bug or
//!    an attack; the agent exits with [`EXIT_CODE`], which releases the input hooks (the core
//!    sees the process end and restarts it in `RECOVERY`).
//!
//! Local IPC (named pipe, unix-domain socket, the display server's own unix socket) is not
//! a network socket and is not reported. The check asks the OS about this process's own
//! sockets: `/proc` on Linux, the extended TCP/UDP tables (`iphlpapi`) on Windows.

use std::time::Duration;

/// How often the running agent re-checks.
const PERIOD: Duration = Duration::from_secs(2);

/// The exit code of an agent that found a network socket of its own.
pub const EXIT_CODE: u8 = 70;

/// Checks once, then keeps checking on a thread of its own.
///
/// `Err` carries the reason and means the agent must not start.
pub fn start() -> Result<(), String> {
    check()?;
    std::thread::Builder::new()
        .name("net-guard".to_owned())
        .spawn(|| loop {
            std::thread::sleep(PERIOD);
            if let Err(violation) = check() {
                tracing::error!(%violation, "agent network guard fired; exiting");
                eprintln!("bifrost agent: {violation}");
                std::process::exit(i32::from(EXIT_CODE));
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start the network guard thread: {error}"))
}

/// `Err` names the first network socket this process owns, or says the check cannot run.
fn check() -> Result<(), String> {
    match find_network_socket()? {
        None => Ok(()),
        Some(socket) => Err(format!(
            "the agent must own no network sockets (rule 1), but this process owns {socket}"
        )),
    }
}

/// A description of one network socket (TCP, UDP or raw) owned by this process, if any.
pub fn find_network_socket() -> Result<Option<String>, String> {
    imp::find_network_socket()
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashSet;

    /// The `/proc/net` tables whose rows carry a socket inode.
    const TABLES: [&str; 6] = ["tcp", "tcp6", "udp", "udp6", "raw", "raw6"];

    pub fn find_network_socket() -> Result<Option<String>, String> {
        let mut inodes = HashSet::new();
        let descriptors = std::fs::read_dir("/proc/self/fd")
            .map_err(|error| format!("cannot list /proc/self/fd: {error}"))?;
        for entry in descriptors.flatten() {
            // A descriptor can close between the listing and the read; that is not an error.
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            {
                inodes.insert(inode);
            }
        }
        if inodes.is_empty() {
            return Ok(None);
        }
        for table in TABLES {
            let path = format!("/proc/self/net/{table}");
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {path}: {error}"))?;
            // Header line, then `sl local rem st tx:rx tr:when retrnsmt uid timeout inode …`.
            for row in text.lines().skip(1) {
                let inode = row
                    .split_whitespace()
                    .nth(9)
                    .and_then(|field| field.parse::<u64>().ok());
                if let Some(inode) = inode.filter(|inode| inodes.contains(inode)) {
                    return Ok(Some(format!("a {table} socket (inode {inode})")));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const NO_ERROR: u32 = 0;

    /// One extended-table query: how to call it and how wide its rows are.
    struct Table {
        name: &'static str,
        family: u32,
        row_size: usize,
        tcp: bool,
    }

    pub fn find_network_socket() -> Result<Option<String>, String> {
        let tables = [
            Table {
                name: "TCP/IPv4",
                family: AF_INET,
                row_size: size_of::<MIB_TCPROW_OWNER_PID>(),
                tcp: true,
            },
            Table {
                name: "TCP/IPv6",
                family: AF_INET6,
                row_size: size_of::<MIB_TCP6ROW_OWNER_PID>(),
                tcp: true,
            },
            Table {
                name: "UDP/IPv4",
                family: AF_INET,
                row_size: size_of::<MIB_UDPROW_OWNER_PID>(),
                tcp: false,
            },
            Table {
                name: "UDP/IPv6",
                family: AF_INET6,
                row_size: size_of::<MIB_UDP6ROW_OWNER_PID>(),
                tcp: false,
            },
        ];
        let pid = std::process::id();
        for table in &tables {
            let buffer = query(table)?;
            // `dwNumEntries`, then that many fixed-width rows; the owning pid is the last
            // field of every row layout.
            let Some(count) = buffer.get(..4) else {
                continue;
            };
            let count = u32::from_ne_bytes([count[0], count[1], count[2], count[3]]) as usize;
            for index in 0..count {
                let end = 4 + (index + 1) * table.row_size;
                let Some(owner) = buffer.get(end - 4..end) else {
                    break;
                };
                let owner = u32::from_ne_bytes([owner[0], owner[1], owner[2], owner[3]]);
                if owner == pid {
                    return Ok(Some(format!("a {} socket", table.name)));
                }
            }
        }
        Ok(None)
    }

    /// The raw table, grown until the OS stops asking for more room (sockets come and go
    /// between the size probe and the read).
    fn query(table: &Table) -> Result<Vec<u8>, String> {
        let mut size = 0u32;
        let mut buffer: Vec<u8> = Vec::new();
        for _ in 0..8 {
            let data = if buffer.is_empty() {
                std::ptr::null_mut()
            } else {
                buffer.as_mut_ptr().cast()
            };
            // SAFETY: `data` is null with `size == 0` (a size probe) or points at `size`
            // writable bytes owned by `buffer`; the call writes at most `size` bytes.
            let status = unsafe {
                if table.tcp {
                    GetExtendedTcpTable(
                        data,
                        &mut size,
                        0,
                        table.family,
                        TCP_TABLE_OWNER_PID_ALL,
                        0,
                    )
                } else {
                    GetExtendedUdpTable(data, &mut size, 0, table.family, UDP_TABLE_OWNER_PID, 0)
                }
            };
            match status {
                NO_ERROR => return Ok(buffer),
                ERROR_INSUFFICIENT_BUFFER => buffer.resize(size as usize + 4096, 0),
                other => {
                    return Err(format!(
                        "cannot read the {} socket table (error {other})",
                        table.name
                    ))
                }
            }
        }
        Err(format!(
            "the {} socket table kept growing while it was read",
            table.name
        ))
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn find_network_socket() -> Result<Option<String>, String> {
        Err("no socket inspection for this platform".to_owned())
    }
}
