//! `lws-core` — the process that owns TLS, the protocol, the router, the focus and
//! ownership FSMs, the link manager and agent supervision.
//!
//! # CLI
//!
//! ```text
//! lws-core [--root DIR] [--config-dir DIR] [--data-dir DIR] [--state-dir DIR]
//!          [--runtime-dir DIR] <command>
//!
//!   run      [--dry-inject] [--dry-input-events N] [--agent PATH]
//!            [--listen IP] [--port-input N] [--port-clipboard N]
//!   pair     (--open [--listen IP:PORT] | --dial IP:PORT)
//!   peers    [--revoke NODE_ID]
//!   status
//!   switch   <peer id|display name>
//! ```
//!
//! `pair` and `peers` work offline against the configuration directory; `status` and
//! `switch` talk to a running core over the local control channel
//! ([`ctl`]). `run` is the daemon: it spawns and supervises `lws-agent`, and it is
//! the only process that listens on the network.

mod agent;
mod command;
mod ctl;
mod pair;
mod routing;
mod session;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use config::Paths;
use focus_router::{FocusRouter, SequentialIds};
use identity::{IdentityPaths, NodeIdentity, TrustStore};
use ipc::Watchdog;
use platform_probe::BackendOverride;
use protocol::ipc::{AgentMode, BackendKind};
use transport::{BackoffBounds, LinkManager, LinkLiveness, ManagerConfig};

use crate::agent::Supervisor;
use crate::session::Session;

/// The framed-IPC version core speaks (`AgentFrame.major`/`minor`).
const FRAME_MAJOR: u32 = protocol::limits::PROTOCOL_MAJOR;

/// Outbound queue capacity per link.
const QUEUE_CAPACITY: usize = 64;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            eprintln!("lws-core: {message}");
            eprintln!("\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Run(error)) => {
            eprintln!("lws-core: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A usage error (exit 2) or a runtime failure (exit 1).
enum Failure {
    Usage(String),
    Run(String),
}

/// Flattens a [`Failure`] into its message, for the paths that return `String`.
fn failure_message(failure: Failure) -> String {
    match failure {
        Failure::Usage(message) | Failure::Run(message) => message,
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Run(message)
    }
}

impl From<config::ConfigError> for Failure {
    fn from(error: config::ConfigError) -> Self {
        Failure::Run(error.to_string())
    }
}

impl From<identity::IdentityError> for Failure {
    fn from(error: identity::IdentityError) -> Self {
        Failure::Run(error.to_string())
    }
}

const USAGE: &str = "\
lws-core — LAN Workspace core process

USAGE:
    lws-core [GLOBAL] <command> [ARGS]

GLOBAL:
    --root DIR          put config/, data/, state/ and run/ under DIR
    --config-dir DIR    override the configuration directory
    --data-dir DIR      override the data directory (identity key)
    --state-dir DIR     override the state directory (logs)
    --runtime-dir DIR   override the runtime directory (control channel)
    --help              print this help

COMMANDS:
    run      run the daemon: links, focus FSM, agent supervision
    pair     pair with a peer: --open (listen) or --dial <ip:port>
    peers    list trusted peers, or --revoke <node id>
    status   ask a running core for its state
    switch   manual transition: to a peer while local, back to local while remote
";

/// The parsed global options and the command line behind them.
struct Args {
    paths: Paths,
    command: String,
    rest: Vec<String>,
}

fn dispatch(argv: &[String]) -> Result<(), Failure> {
    let mut root: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut runtime_dir: Option<PathBuf> = None;

    let mut index = 0;
    let mut command = None;
    while index < argv.len() {
        let argument = argv[index].as_str();
        match argument {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--root" | "--config-dir" | "--data-dir" | "--state-dir" | "--runtime-dir" => {
                let value = argv
                    .get(index + 1)
                    .ok_or_else(|| Failure::Usage(format!("{argument} needs a value")))?;
                let path = PathBuf::from(value);
                match argument {
                    "--root" => root = Some(path),
                    "--config-dir" => config_dir = Some(path),
                    "--data-dir" => data_dir = Some(path),
                    "--state-dir" => state_dir = Some(path),
                    "--runtime-dir" => runtime_dir = Some(path),
                    _ => unreachable!("the match above lists every flag"),
                }
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown option {other:?}")));
            }
            other => {
                command = Some(other.to_owned());
                index += 1;
                break;
            }
        }
    }

    let command = command.ok_or_else(|| Failure::Usage("no command given".to_owned()))?;
    let rest = argv[index..].to_vec();
    let paths = resolve_paths(root, config_dir, data_dir, state_dir, runtime_dir)?;
    let args = Args {
        paths,
        command,
        rest,
    };

    match args.command.as_str() {
        "run" => run(&args),
        "pair" => pair::main(&args.paths, &args.rest),
        "peers" => peers(&args),
        "status" => control(&args.paths, "status"),
        "switch" => {
            let peer = args
                .rest
                .first()
                .ok_or_else(|| Failure::Usage("switch needs a peer".to_owned()))?;
            control(&args.paths, &format!("switch {peer}"))
        }
        other => Err(Failure::Usage(format!("unknown command {other:?}"))),
    }
}

/// Resolves the four directories, honouring `--root` and the documented defaults.
fn resolve_paths(
    root: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
) -> Result<Paths, Failure> {
    let defaults = config::Config::default_paths()?;
    let under_root = |name: &str| root.as_ref().map(|root| root.join(name));
    Ok(Paths::new(
        config_dir
            .or_else(|| under_root("config"))
            .unwrap_or_else(|| defaults.config_dir().to_path_buf()),
        data_dir
            .or_else(|| under_root("data"))
            .unwrap_or_else(|| defaults.data_dir().to_path_buf()),
        state_dir
            .or_else(|| under_root("state"))
            .unwrap_or_else(|| defaults.state_dir().to_path_buf()),
        runtime_dir
            .or_else(|| under_root("run"))
            .or_else(|| defaults.runtime_dir().map(std::path::Path::to_path_buf)),
    ))
}

/// Starts the logger at the level the configuration asks for.
fn init_logging(paths: &Paths) {
    let level = std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|text| config::Config::from_str(&text).ok())
        .map(|config| match config.log.level {
            config::LogLevel::Trace => "trace",
            config::LogLevel::Debug => "debug",
            config::LogLevel::Info => "info",
            config::LogLevel::Warn => "warn",
            config::LogLevel::Error => "error",
        })
        .unwrap_or("info");
    let filter = tracing_subscriber::EnvFilter::try_from_env("LWS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

/// `run`: the daemon.
fn run(args: &Args) -> Result<(), Failure> {
    init_logging(&args.paths);
    let options = RunOptions::parse(&args.rest)?;
    let paths = &args.paths;

    let config = config::Config::load(paths.config_file())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))?;
    runtime
        .block_on(async move { run_async(paths, options, config).await })
        .map_err(Failure::Run)
}

/// The daemon's async half, so `run` only owns the runtime and the exit mapping.
async fn run_async(
    paths: &Paths,
    options: RunOptions,
    config: config::Config,
) -> Result<(), String> {
    let identity_paths = IdentityPaths {
        data_dir: paths.data_dir().to_path_buf(),
        key_file: paths.identity_file(),
        trust_file: paths.trust_file(),
    };
    let (identity, load) =
        NodeIdentity::load_or_create(&identity_paths).map_err(|error| error.to_string())?;
    tracing::info!(load = ?load, "identity loaded");
    let trust = load_trust(paths).map_err(failure_message)?;
    let session = decide_session(&config);
    let node_id = identity.node_id();

    let listen: SocketAddr = options
        .listen
        .clone()
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                config.network.listen_address, config.network.port_input
            )
        })
        .parse()
        .map_err(|error| format!("listen address: {error}"))?;
    let mut peers = dial_targets(&config, &trust).map_err(failure_message)?;
    peers.sort();
    peers.dedup();

    let local_hello = routing::hello(
        node_id,
        config.node.display_name.as_str(),
        config.node.can_own,
        session.capabilities,
        session.session_kind,
        routing::Gossip {
            owner_serial: config.workspace.owner_claim_serial,
            owner: owner_of(&config),
            topology_revision: config.topology.revision,
            displays_revision: 0,
        },
    );
    let manager_config = ManagerConfig {
        identity: Arc::new(identity),
        trust: trust.clone(),
        local_hello,
        listen,
        peers,
        max_members: config.network.max_members,
        queue_capacity: QUEUE_CAPACITY,
        // The liveness cadence and the backoff bounds are settings, not transport
        // constants: `crates/config` validates both (`link_timeout_ms > link_ping_ms`,
        // `500 <= backoff <= 5000`) and the link layer only uses what it is handed.
        liveness: LinkLiveness {
            ping_ms: u64::from(config.workspace.link_ping_ms),
            offline_ms: u64::from(config.workspace.link_timeout_ms),
        },
        backoff: BackoffBounds {
            min_ms: config.network.reconnect_backoff_ms[0],
            max_ms: config.network.reconnect_backoff_ms[1],
        },
    };
    let (manager, link_events) =
        LinkManager::new(manager_config).map_err(|error| format!("link manager: {error}"))?;
    let manager = Arc::new(manager);
    let runner = Arc::clone(&manager);
    tokio::spawn(async move {
        if let Err(error) = runner.run().await {
            tracing::error!(%error, "the link manager stopped");
        }
    });

    // The sibling change to `focus_router::Timing` removed `repeat_guard_ms`; the
    // literal names every field it has today, so a new field would break the build
    // here rather than silently take a default.
    let timing = focus_router::Timing {
        prepare_timeout_ms: u64::from(config.timing.prepare_timeout_ms),
        lease_ttl_ms: u64::from(config.timing.lease_ttl_ms),
        claim_grant_timeout_ms: u64::from(config.workspace.claim_grant_timeout_ms),
    };
    let mut router = FocusRouter::new(node_id, timing, SequentialIds::default(), session.fsm)
        .with_claim_on_startup(config.node.claim_on_startup)
        .with_require_emergency_hotkey(config.input.require_emergency_hotkey);
    if let Some(owner) = owner_pair(&config) {
        router = router.with_persisted_ownership(config.workspace.owner_claim_serial, Some(owner));
    }

    let endpoint =
        ctl::endpoint(private_root(paths)).map_err(|error| format!("control endpoint: {error}"))?;
    let server = ctl::Server::bind(&endpoint)
        .map_err(|error| format!("cannot own {}: {error}", endpoint.describe()))?;
    let watchdog = Watchdog::for_core_silence_ms(config.timing.agent_core_silence_ms, 0)
        .map_err(|error| format!("agent_core_silence_ms: {error}"))?;

    let mut supervisor = Supervisor::new(
        options.agent.clone(),
        private_root(paths).to_path_buf(),
        private_root(paths).to_path_buf(),
        options.dry_inject,
        options.dry_input_events,
        config.input.emergency_hotkey.clone(),
        backend_kind(&config),
        config.input.require_emergency_hotkey,
        config.timing.agent_core_silence_ms,
    );
    supervisor.start(AgentMode::Normal).await?;

    tracing::info!(
        listen = listen.to_string().as_str(),
        backend = session.backend.as_str(),
        session_kind = session.session_kind.as_str_name(),
        "starting"
    );
    let mut session = Session::new(
        router,
        manager,
        link_events,
        supervisor,
        server,
        watchdog,
        node_id,
        session.backend,
        trust,
    );
    session.run().await.map_err(|error| error.to_string())
}

/// The `run`-only options.
struct RunOptions {
    dry_inject: bool,
    dry_input_events: usize,
    agent: PathBuf,
    listen: Option<String>,
}

impl RunOptions {
    fn parse(rest: &[String]) -> Result<Self, Failure> {
        let mut dry_inject = false;
        let mut dry_input_events = 0usize;
        let mut agent = None;
        let mut listen = None;
        let mut index = 0;
        while index < rest.len() {
            let argument = rest[index].as_str();
            match argument {
                "--dry-inject" => {
                    dry_inject = true;
                    index += 1;
                }
                "--dry-input-events" => {
                    let value = rest.get(index + 1).ok_or_else(|| {
                        Failure::Usage("--dry-input-events needs a value".to_owned())
                    })?;
                    dry_input_events = value
                        .parse()
                        .map_err(|_| Failure::Usage(format!("{value} is not a number")))?;
                    index += 2;
                }
                "--agent" => {
                    agent =
                        Some(PathBuf::from(rest.get(index + 1).ok_or_else(|| {
                            Failure::Usage("--agent needs a path".to_owned())
                        })?));
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
                other => return Err(Failure::Usage(format!("unknown run option {other:?}"))),
            }
        }
        let agent = match agent {
            Some(agent) => agent,
            None => agent::default_agent_program().map_err(Failure::Run)?,
        };
        Ok(RunOptions {
            dry_inject,
            dry_input_events,
            agent,
            listen,
        })
    }
}

/// The private directory the launch token lives in.
fn private_root(paths: &Paths) -> &std::path::Path {
    paths.runtime_dir().unwrap_or_else(|| paths.state_dir())
}

/// The local probe, reduced to what the core needs.
struct LocalSession {
    capabilities: platform_probe::Capabilities,
    fsm: focus_router::Capabilities,
    session_kind: protocol::wire::SessionKind,
    backend: String,
}

/// Probes this host.
///
/// `read_snapshot` is Linux-only; on Windows the host OS is the evidence, exactly as
/// in the agent.
fn decide_session(config: &config::Config) -> LocalSession {
    #[cfg(target_os = "linux")]
    let snapshot = platform_probe::read_snapshot();
    #[cfg(not(target_os = "linux"))]
    let snapshot = platform_probe::EnvSnapshot::new(platform_probe::HostOs::Windows);

    let requested = match config.platform.backend {
        config::Backend::X11 => BackendOverride::X11,
        config::Backend::Wayland => BackendOverride::Wayland,
        config::Backend::Auto => BackendOverride::Auto,
    };
    match platform_probe::decide(&snapshot, requested) {
        Ok(session) => LocalSession {
            capabilities: session.capabilities,
            fsm: focus_router::Capabilities {
                capture: session.capabilities.capture,
                inject: session.capabilities.inject,
                absolute_pointer: session.capabilities.absolute_pointer,
                emergency_hotkey: session.capabilities.emergency_hotkey,
            },
            session_kind: wire_session_kind(session.session_kind),
            backend: session.backend.name().to_owned(),
        },
        Err(unsupported) => {
            tracing::warn!(reason = %unsupported, "unsupported session: this node stays local");
            LocalSession {
                capabilities: no_capabilities(),
                fsm: focus_router::Capabilities::default(),
                session_kind: protocol::wire::SessionKind::Unspecified,
                backend: String::new(),
            }
        }
    }
}

/// Every capability off, for a session this build cannot drive.
///
/// `platform_probe::Capabilities` has no `Default` on purpose: a capability is a
/// claim, and the empty claim has to be written out.
fn no_capabilities() -> platform_probe::Capabilities {
    platform_probe::Capabilities {
        capture: false,
        inject: false,
        absolute_pointer: false,
        physical_hid_keys: false,
        text_clipboard: false,
        native_repeat: false,
        software_repeat: false,
        receiver_takeover: false,
        displays_changed: false,
        emergency_hotkey: false,
    }
}

fn wire_session_kind(kind: platform_probe::SessionKind) -> protocol::wire::SessionKind {
    match kind {
        platform_probe::SessionKind::Windows => protocol::wire::SessionKind::Windows,
        platform_probe::SessionKind::X11 => protocol::wire::SessionKind::X11,
        platform_probe::SessionKind::Wayland => protocol::wire::SessionKind::Wayland,
    }
}

fn backend_kind(config: &config::Config) -> BackendKind {
    match config.platform.backend {
        config::Backend::Auto => BackendKind::Auto,
        config::Backend::X11 => BackendKind::X11,
        config::Backend::Wayland => BackendKind::Wayland,
    }
}

/// The persisted owner pair, when the configuration names one.
fn owner_pair(config: &config::Config) -> Option<(u64, geometry::NodeId)> {
    let bytes = config::topology::node_id_from_hex(&config.workspace.owner_node_id)?;
    if config.workspace.owner_claim_serial == 0 {
        return None;
    }
    Some((
        config.workspace.owner_claim_serial,
        geometry::NodeId::new(bytes),
    ))
}

fn owner_of(config: &config::Config) -> Option<geometry::NodeId> {
    owner_pair(config).map(|(_, node_id)| node_id)
}

/// The addresses to dial: the configured peers plus every trusted peer's last known
/// address («адреса даёт сопряжение»).
fn dial_targets(config: &config::Config, trust: &TrustStore) -> Result<Vec<SocketAddr>, Failure> {
    let port = config.network.port_input;
    let mut addresses = Vec::new();
    for peer in &config.network.peers {
        let address: SocketAddr = peer
            .address
            .parse()
            .map_err(|error| Failure::Run(format!("peer address {}: {error}", peer.address)))?;
        addresses.push(address);
    }
    for peer in trust.active() {
        let Some(last) = peer.last_address.as_ref() else {
            continue;
        };
        match last.parse::<SocketAddr>() {
            Ok(address) => addresses.push(address),
            Err(_) => match last.parse::<std::net::IpAddr>() {
                Ok(ip) => addresses.push(SocketAddr::new(ip, port)),
                Err(error) => {
                    tracing::warn!(address = last.as_str(), %error, "unusable peer address");
                }
            },
        }
    }
    Ok(addresses)
}

/// Loads `trust.toml`, starting with an empty store when it is missing or damaged.
fn load_trust(paths: &Paths) -> Result<TrustStore, Failure> {
    let path = paths.trust_file();
    match TrustStore::load(&path) {
        Ok(identity::atomic::LoadResult::Missing) => Ok(TrustStore::new()),
        Ok(identity::atomic::LoadResult::Loaded(store)) => Ok(store),
        Ok(identity::atomic::LoadResult::Recovered {
            value,
            primary_error,
            backup,
        }) => {
            tracing::warn!(
                primary_error = primary_error.as_str(),
                backup = backup.display().to_string().as_str(),
                "trust.toml was damaged; the backup was loaded"
            );
            Ok(value)
        }
        Ok(identity::atomic::LoadResult::Corrupt { primary_error }) => {
            // «Если повреждены оба файла — дефолты, владелец не заявлен»: start
            // untrusted and stay local until the user pairs again.
            tracing::error!(
                primary_error = primary_error.as_str(),
                "trust.toml and its backup are unreadable; starting with no peers"
            );
            Ok(TrustStore::new())
        }
        Err(error) => Err(Failure::Run(error.to_string())),
    }
}

/// `peers`: list or revoke, offline.
fn peers(args: &Args) -> Result<(), Failure> {
    init_logging(&args.paths);
    let paths = &args.paths;
    let identity_paths = IdentityPaths {
        data_dir: paths.data_dir().to_path_buf(),
        key_file: paths.identity_file(),
        trust_file: paths.trust_file(),
    };
    let (identity, _) = NodeIdentity::load_or_create(&identity_paths)?;
    let mut trust = load_trust(paths)?;

    let revoke = match args.rest.as_slice() {
        [] => None,
        [flag, id] if flag == "--revoke" => Some(id.clone()),
        _ => {
            return Err(Failure::Usage(
                "peers takes nothing or --revoke <node id>".to_owned(),
            ))
        }
    };

    if let Some(id) = revoke {
        let reference = command::PeerRef::new(id);
        let node_id = command::resolve_peer(&trust, &reference).map_err(Failure::Run)?;
        let removed = trust.revoke(&node_id, identity::now_unix_ms());
        if removed {
            trust.save(&paths.trust_file())?;
            println!("revoked {}", command::node_id_hex(&node_id));
        } else {
            println!("nothing to revoke for {}", command::node_id_hex(&node_id));
        }
        return Ok(());
    }

    println!("node {}", command::node_id_hex(&identity.node_id()));
    println!("fingerprint {}", identity.fingerprint_grouped());
    if trust.is_empty() {
        println!("no trusted peers");
        return Ok(());
    }
    for peer in &trust.peers {
        println!(
            "{} {} {} {}",
            if peer.is_active() { "peer" } else { "revoked" },
            command::node_id_hex(&peer.node_id),
            peer.display_name.as_str(),
            peer.last_address.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// `status`/`switch`: one request to a running core.
fn control(paths: &Paths, line: &str) -> Result<(), Failure> {
    init_logging(paths);
    let endpoint =
        ctl::endpoint(private_root(paths)).map_err(|error| format!("control endpoint: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))?;
    let reply = runtime.block_on(async move {
        tokio::time::timeout(Duration::from_secs(5), ctl::request(&endpoint, line))
            .await
            .map_err(|_| "the running core did not answer".to_owned())?
            .map_err(|error| error.to_string())
    })?;
    println!("{}", reply.trim_end());
    match ctl::Response::parse(&reply) {
        Ok(ctl::Response::Error { code, .. }) => Err(Failure::Run(code)),
        _ => Ok(()),
    }
}
