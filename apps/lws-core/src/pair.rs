//! `lws-core pair`: the CLI half of pairing (KVM-030).
//!
//! `identity::drive` never prints the SAS and never persists anything: the caller
//! shows the code, takes the decision and commits the peer
//! («Однократное взаимное подтверждение ключей; результат — TrustedPeer»). This
//! module is that caller, and it is the only place in the slice where a human has to
//! type `y` — the smoke pipes it in.
//!
//! Both sides must answer, because the ceremony is mutual: `approve` sends the local
//! decision and waits for the peer's, and only a `Committed` outcome is written to
//! `trust.toml`. A rejection writes nothing.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use config::Paths;
use identity::{PairedPeer, PairingDriver, TrustStore};

use crate::{Failure, USAGE};

/// How long the dialer waits for the opener to appear.
const WINDOW: Duration = Duration::from_secs(120);

/// The documented SAS window (`identity::pairing::WINDOW_MS` is 120 s).
pub fn main(paths: &Paths, rest: &[String]) -> Result<(), Failure> {
    crate::init_logging(paths);
    let mut open = false;
    let mut dial: Option<String> = None;
    let mut listen: Option<String> = None;

    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--open" => {
                open = true;
                index += 1;
            }
            "--dial" => {
                dial = Some(
                    rest.get(index + 1)
                        .ok_or_else(|| Failure::Usage("--dial needs an address".to_owned()))?
                        .clone(),
                );
                index += 2;
            }
            "--listen" => {
                listen = Some(
                    rest.get(index + 1)
                        .ok_or_else(|| Failure::Usage("--listen needs an address".to_owned()))?
                        .clone(),
                );
                index += 2;
            }
            other => {
                return Err(Failure::Usage(format!(
                    "unknown pair option {other:?}\n\n{USAGE}"
                )))
            }
        }
    }
    if open == dial.is_some() {
        return Err(Failure::Usage(
            "pair takes exactly one of --open or --dial <address>".to_owned(),
        ));
    }

    let identity_paths = identity::IdentityPaths {
        data_dir: paths.data_dir().to_path_buf(),
        key_file: paths.identity_file(),
        trust_file: paths.trust_file(),
    };
    let (identity, _) = identity::NodeIdentity::load_or_create(&identity_paths)?;
    let mut trust = crate::load_trust(paths)?;
    let trust_path = paths.trust_file();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))?;

    runtime.block_on(async move {
        let driver = if open {
            let address: SocketAddr = listen
                .unwrap_or_else(|| "127.0.0.1:47911".to_owned())
                .parse()
                .map_err(|error| Failure::Run(format!("--listen: {error}")))?;
            let mut window = identity::open_window(identity::now_unix_ms() as u64)
                .map_err(|error| Failure::Run(error.to_string()))?;
            let listener = identity::bind(address)
                .await
                .map_err(|error| Failure::Run(error.to_string()))?;
            println!(
                "listening on {address} as {}; waiting for a peer to dial (window {}s)",
                identity.fingerprint_grouped(),
                WINDOW.as_secs()
            );
            identity::accept_one(
                &listener,
                &identity,
                &mut window,
                identity::now_unix_ms() as u64,
            )
            .await
            .map_err(|error| Failure::Run(error.to_string()))?
        } else {
            let address: SocketAddr = dial
                .expect("checked above")
                .parse()
                .map_err(|error| Failure::Run(format!("--dial: {error}")))?;
            identity::dial(address, &identity, WINDOW)
                .await
                .map_err(|error| Failure::Run(error.to_string()))?
        };
        confirm(driver, &mut trust, &trust_path, open).await
    })
}

/// Shows the SAS, asks, and commits only a mutual approval.
async fn confirm(
    driver: PairingDriver,
    trust: &mut TrustStore,
    trust_path: &Path,
    opener: bool,
) -> Result<(), Failure> {
    println!("SAS: {}", driver.sas().text());
    println!("peer: {}", driver.peer_display_name());
    println!("peer fingerprint: {}", driver.peer_fingerprint());
    println!("compare the six digits with the other machine, then answer y/n");
    print!("codes match? [y/N] ");
    std::io::stdout().flush().ok();

    let address = peer_address(opener, driver.address());
    let approved = ask().await?;
    if !approved {
        driver
            .reject()
            .await
            .map_err(|error| Failure::Run(error.to_string()))?;
        println!("rejected; nothing was written");
        return Ok(());
    }
    let paired: PairedPeer = driver
        .approve()
        .await
        .map_err(|error| Failure::Run(error.to_string()))?;
    let record = paired.trusted_peer(identity::now_unix_ms(), address);
    let node_id = record.node_id;
    trust.trust(record);
    trust.save(trust_path)?;
    println!(
        "paired with {} ({})",
        paired.display_name,
        crate::command::node_id_hex(&node_id)
    );
    Ok(())
}

/// The address to remember for the peer.
///
/// The dialer typed the opener's address, so the address it observed *is* the peer's input
/// endpoint and its port is real. The opener only sees the dialer's *source* address, whose
/// port is ephemeral, and `PairHello` carries no address at all (`docs/protocol.proto`:
/// `node_id`, `display_name`, `supports_sas`), so the only address it can record honestly is
/// the dialer's IP with the documented input port (`docs/05-protocol.md`: «Адреса для
/// `network.peers` берутся из сопряжения, поэтому ручной IP вводится один раз на пару»).
fn peer_address(opener: bool, observed: Option<&str>) -> Option<String> {
    let mut address: SocketAddr = observed?.parse().ok()?;
    if opener {
        address.set_port(protocol::PORT_INPUT_CONTROL);
    }
    Some(address.to_string())
}

/// Reads one y/n answer from stdin without blocking the runtime.
async fn ask() -> Result<bool, Failure> {
    tokio::task::spawn_blocking(|| {
        let mut answer = String::new();
        match std::io::stdin().read_line(&mut answer) {
            Ok(0) => false,
            Ok(_) => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            Err(_) => false,
        }
    })
    .await
    .map_err(|error| Failure::Run(format!("cannot read the answer: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opener_records_the_documented_port_and_the_dialer_keeps_the_typed_one() {
        // The dialer typed the opener's address: that port is the peer's input port.
        assert_eq!(
            peer_address(false, Some("192.168.1.11:47931")),
            Some("192.168.1.11:47931".to_owned())
        );

        // The opener saw an ephemeral source port, and `PairHello` has no address field,
        // so it records the documented input port instead of an unusable one.
        assert_eq!(
            peer_address(true, Some("192.168.1.11:56175")),
            Some(format!("192.168.1.11:{}", protocol::PORT_INPUT_CONTROL))
        );

        assert_eq!(peer_address(true, None), None);
        assert_eq!(
            peer_address(false, Some("not an address")),
            None,
            "an unparsable address is not stored"
        );
    }
}
