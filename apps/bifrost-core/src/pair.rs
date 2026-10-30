//! `bifrost-core pair`: the CLI half of pairing (KVM-030).
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
//!
//! # With a core already running
//!
//! The peer's `pair/1` connection arrives on *core's* link listener (`docs/05-protocol.md`:
//! pairing shares `47901`, told apart by ALPN), so a window can only be served by the process
//! that owns that listener. `pair --open` therefore asks the running core over its control
//! channel (`pair-open`/`pair-prompt`/`pair-answer`) and only binds a listener of its own when
//! no core holds the port — the `--listen` address defaults to the configured
//! `network.listen_address:port_input` for exactly that case.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use config::Paths;
use identity::{PairedPeer, PairingDriver, TrustStore};

use crate::{Failure, USAGE};

/// How long the dialer waits for the opener to appear.
pub(crate) const WINDOW: Duration = Duration::from_secs(120);

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

    // The port this node listens on. `PairHello` advertises it so the *opener* can record
    // an address for a dialer that does not use the documented default: the opener only
    // sees the dialer's ephemeral source port. A missing or invalid configuration is not
    // fatal here — `pair` reads this one setting and nothing else from it.
    // The port this node listens on, and the address a window opens on when this CLI has to
    // serve one itself: `network.listen_address` + `network.port_input`, which is where the
    // peer dials (`docs/05-protocol.md`). A missing or invalid configuration is not fatal
    // here — this command reads these two settings and nothing else from it.
    let configured = config::Config::load(paths.config_file()).ok();
    let listen_port = configured
        .as_ref()
        .map(|config| config.network.port_input)
        .unwrap_or(protocol::PORT_INPUT_CONTROL);
    let default_listen = configured.as_ref().map_or_else(
        || format!("127.0.0.1:{listen_port}"),
        |config| {
            format!(
                "{}:{}",
                config.network.listen_address, config.network.port_input
            )
        },
    );
    // What the other computer's prompt and trust record show for this node. With no readable
    // configuration there is no configured name, and the documented default stands in for it
    // rather than an invented one.
    let display_name = configured.as_ref().map_or_else(
        || config::Config::default().node.display_name,
        |config| config.node.display_name.clone(),
    );

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
            // A core that is already running owns `pair/1`: open the window through it.
            let through_core = if listen.is_none() {
                window_on_running_core(paths).await?
            } else {
                None
            };
            if let Some(outcome) = through_core {
                return outcome;
            }
            let address: SocketAddr = listen
                .unwrap_or(default_listen)
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
                listen_port,
                &display_name,
            )
            .await
            .map_err(|error| Failure::Run(error.to_string()))?
        } else {
            let address: SocketAddr = dial
                .expect("checked above")
                .parse()
                .map_err(|error| Failure::Run(format!("--dial: {error}")))?;
            identity::dial(address, &identity, WINDOW, listen_port, &display_name)
                .await
                .map_err(|error| Failure::Run(error.to_string()))?
        };
        confirm(driver, &mut trust, &trust_path, open).await
    })
}

/// Opens the window on a core that is already running, if there is one.
///
/// `None` means "no core is listening": the caller then serves the window itself. `Some(())`
/// means the window was served here, through the core, and the command is finished.
async fn window_on_running_core(paths: &Paths) -> Result<Option<Result<(), Failure>>, Failure> {
    let endpoint = crate::ctl::endpoint(crate::private_root(paths))
        .map_err(|error| Failure::Run(format!("control endpoint: {error}")))?;
    // `ping` is the documented liveness probe of the control channel and touches nothing
    // else; a refused connection means no core holds this configuration directory.
    if crate::ctl::request(&endpoint, "ping").await.is_err() {
        return Ok(None);
    }
    let opened = ask_core(&endpoint, "pair-open 120", WINDOW)
        .await
        .map_err(Failure::Run)?;
    let fields = Fields::parse(&opened);
    println!(
        "ядро уже запущено: окно открыто через него.\nадрес для второго компьютера: {}",
        fields.get("address").unwrap_or("?")
    );
    println!(
        "ожидание подключения второго компьютера ({} с)…",
        fields.get("seconds").unwrap_or("120")
    );

    let prompt = ask_core(&endpoint, "pair-prompt", WINDOW + Duration::from_secs(10))
        .await
        .map_err(Failure::Run)?;
    let fields = Fields::parse(&prompt);
    println!("SAS: {}", fields.get("sas").unwrap_or("?"));
    println!("peer: {}", fields.get("peer").unwrap_or("?"));
    println!(
        "{}",
        sas_confirmation_guidance(
            fields
                .get("sas_ceremony_version")
                .and_then(|value| value.parse().ok())
        )
    );
    print!("codes match? [y/N] ");
    std::io::stdout().flush().ok();

    let approved = ask().await?;
    let Some(ceremony) = fields.get("ceremony") else {
        return Err(Failure::Run(
            "the core did not name the ceremony id".to_owned(),
        ));
    };
    let answer = format!(
        "pair-answer {ceremony} {}",
        if approved { "approve" } else { "reject" }
    );
    let outcome = ask_core(&endpoint, &answer, WINDOW + Duration::from_secs(10))
        .await
        .map_err(Failure::Run)?;
    println!("{outcome}");
    Ok(Some(Ok(())))
}

/// One control request with a deadline, because the deferred pairing verbs answer when the
/// ceremony gets there and never before.
async fn ask_core(
    endpoint: &ipc::transport::Endpoint,
    line: &str,
    within: Duration,
) -> Result<String, String> {
    match tokio::time::timeout(within, crate::ctl::request(endpoint, line)).await {
        Ok(Ok(reply)) => {
            if let Some(rest) = reply.strip_prefix("ERR ") {
                Err(rest.to_owned())
            } else {
                Ok(reply.trim_start_matches("OK ").to_owned())
            }
        }
        Ok(Err(error)) => Err(format!("{error}")),
        Err(_) => Err(format!(
            "the core did not answer {line:?} within {within:?}"
        )),
    }
}

/// The `key=value` fields of a control reply.
struct Fields(Vec<(String, String)>);

impl Fields {
    fn parse(line: &str) -> Self {
        Fields(
            line.split_whitespace()
                .filter_map(|field| field.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
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
    println!(
        "{}",
        sas_confirmation_guidance(Some(driver.sas_ceremony_version()))
    );
    print!("codes match? [y/N] ");
    std::io::stdout().flush().ok();

    let address = peer_address(opener, driver.address(), driver.peer_listen_port());
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

/// Old or unspecified ceremonies must not appear to offer v1 SAS protection.
fn sas_confirmation_guidance(version: Option<u32>) -> &'static str {
    if version == Some(1) {
        "compare the six digits with the other machine, then answer y/n"
    } else {
        "WARNING: legacy SAS pairing does not protect against active attacks. Upgrade both computers before pairing.\ncompare the six digits with the other machine, then answer y/n"
    }
}

/// The address to remember for the peer.
///
/// The dialer typed the opener's address, so the address it observed *is* the peer's input
/// endpoint and its port is real. The opener only ever sees the dialer's *source* address,
/// whose port is ephemeral, so it takes the IP from the socket and the port from
/// `PairHello.listen_port` — the port the dialer actually listens on, or the documented
/// default when it sent `0` (`docs/05-protocol.md`: «Адреса для `network.peers` берутся из
/// сопряжения, поэтому ручной IP вводится один раз на пару»).
///
/// The GUI's ceremonies use this too (`crate::session::pairing`), so both surfaces record
/// the same address for the same peer.
pub(crate) fn peer_address(
    opener: bool,
    observed: Option<&str>,
    listen_port: u16,
) -> Option<String> {
    let mut address: SocketAddr = observed?.parse().ok()?;
    if opener {
        address.set_port(if listen_port == 0 {
            protocol::PORT_INPUT_CONTROL
        } else {
            listen_port
        });
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
    fn legacy_and_unknown_sas_versions_warn_before_confirmation() {
        for version in [None, Some(0), Some(2)] {
            let guidance = sas_confirmation_guidance(version);
            assert!(guidance.contains("legacy"));
            assert!(guidance.contains("does not protect against active attacks"));
            assert!(guidance.contains("compare the six digits"));
        }
        let guidance = sas_confirmation_guidance(Some(1));
        assert!(!guidance.contains("legacy"));
        assert!(guidance.contains("compare the six digits"));
    }

    #[test]
    fn running_core_prompt_answers_the_ceremony_not_the_window_attempt() {
        const CHILD: &str = "BIFROST_PAIR_PROMPT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pair::tests::running_core_prompt_answers_the_ceremony_not_the_window_attempt",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = std::env::temp_dir().join(format!(
                    "bifrost-pair-prompt-{}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&root).unwrap();
                let paths = Paths::new(&root, &root, &root, Some(root.clone()));
                let endpoint = crate::ctl::endpoint(crate::private_root(&paths)).unwrap();
                let mut server = crate::ctl::Server::bind(&endpoint).unwrap();
                let peer = tokio::spawn(async move {
                    for (expected, reply) in [
                        ("ping", "OK pong"),
                        (
                            "pair-open 120",
                            "OK address=127.0.0.1:47901 attempt=11111111111111111111111111111111 seconds=120",
                        ),
                        (
                            "pair-prompt",
                            "OK sas=123456 peer=LAPTOP ceremony=22222222222222222222222222222222 address=127.0.0.1:47901",
                        ),
                        (
                            "pair-answer 22222222222222222222222222222222 reject",
                            "OK rejected",
                        ),
                    ] {
                        let mut stream = server.accept().await.unwrap();
                        let line = crate::ctl::read_line(&mut stream).await.unwrap().unwrap();
                        assert_eq!(line.trim(), expected);
                        crate::ctl::write_line(&mut stream, reply).await.unwrap();
                    }
                });
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    window_on_running_core(&paths),
                )
                .await
                .expect("the local pairing exchange completes");
                assert!(matches!(result, Ok(Some(Ok(())))));
                peer.await.unwrap();
                std::fs::remove_dir_all(root).unwrap();
            });
    }

    #[test]
    fn the_opener_records_the_documented_port_and_the_dialer_keeps_the_typed_one() {
        // The dialer typed the opener's address: that port is the peer's input port.
        assert_eq!(
            peer_address(false, Some("192.168.1.11:47931"), 0),
            Some("192.168.1.11:47931".to_owned())
        );

        // The opener saw an ephemeral source port; `0` means the dialer listens on the
        // documented default.
        assert_eq!(
            peer_address(true, Some("192.168.1.11:56175"), 0),
            Some(format!("192.168.1.11:{}", protocol::PORT_INPUT_CONTROL))
        );

        // A dialer that listens elsewhere says so in `PairHello`, and the opener records
        // that port instead of a dead one.
        assert_eq!(
            peer_address(true, Some("192.168.1.11:56175"), 47931),
            Some("192.168.1.11:47931".to_owned())
        );

        assert_eq!(peer_address(true, None, 47931), None);
        assert_eq!(
            peer_address(false, Some("not an address"), 47931),
            None,
            "an unparsable address is not stored"
        );
    }
}
