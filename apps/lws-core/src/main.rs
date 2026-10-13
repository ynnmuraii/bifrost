//! `lws-core` — the process that owns TLS, the protocol, the router, the focus and
//! ownership FSMs, the link manager and agent supervision.
//!
//! # CLI
//!
//! ```text
//! lws-core [--root DIR] [--config-dir DIR] [--data-dir DIR] [--state-dir DIR]
//!          [--runtime-dir DIR] <command>
//!
//!          run      [--dev-capture-injected] [--dry-inject] [--dry-displays] [--dry-input-events N] [--agent PATH]
//!                   [--listen IP] [--port-input N] [--port-clipboard N]
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
mod clipboard;
mod command;
mod ctl;
mod gui;
mod ids;
mod job;
mod layout;
mod layout_store;
mod ownership_store;
mod pair;
mod portal;
mod routing;
mod session;
mod topology;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use config::Paths;
use focus_router::FocusRouter;
use identity::{IdentityPaths, NodeIdentity, TrustStore};
use platform_probe::BackendOverride;
use protocol::ipc::{AgentMode, BackendKind};
use transport::{BackoffBounds, LinkLiveness, LinkManager, ManagerConfig};

use crate::agent::Supervisor;
use crate::layout_store::LayoutStore;
use crate::ownership_store::OwnershipStore;
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
    claim    take ownership of the workspace on this node
    abandon  give up ownership and stay a follower
    layout   the layout document: show | edit <file> | apply | discard
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

    // A command that writes must be able to start with an empty root (`--root DIR`
    // promises a self-contained tree): the launcher writes its token into `run/`, the
    // identity and the trust store into `data/`/`config/`, logs into `state/`, and all of
    // them are owner-only («ACL на текущий SID в Windows», 0700 on Linux).
    if matches!(args.command.as_str(), "run" | "pair" | "peers") {
        prepare_dirs(&args.paths)?;
    }

    match args.command.as_str() {
        "run" => run(&args),
        "pair" => pair::main(&args.paths, &args.rest),
        "peers" => peers(&args),
        "status" => control(&args.paths, "status"),
        // KVM-072: the layout document's CAS surface, until the GUI owns it.
        "layout" => {
            let verb = args.rest.first().ok_or_else(|| {
                Failure::Usage("layout takes show, edit <file>, apply, discard, or auto".to_owned())
            })?;
            let line = match verb.as_str() {
                "edit" => {
                    let path = args
                        .rest
                        .get(1)
                        .ok_or_else(|| Failure::Usage("layout edit needs a file".to_owned()))?;
                    format!("layout edit {path}")
                }
                other => format!("layout {other}"),
            };
            control(&args.paths, &line)
        }
        "switch" => {
            let peer = args
                .rest
                .first()
                .ok_or_else(|| Failure::Usage("switch needs a peer".to_owned()))?;
            control(&args.paths, &format!("switch {peer}"))
        }
        // `GuiFrame.ClaimOwnership`/`AbandonOwnership` through the same line protocol the
        // GUI will use; the three-node harness claims from a script with these.
        "claim" => control(&args.paths, "claim"),
        "abandon" => control(&args.paths, "abandon"),
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

/// Starts the logger at the level the configuration asks for, writing to stderr **and** to
/// the rotating file `docs/03-architecture.md` documents (`<logs>/lws-core.log`).
///
/// The file matters on the trial build: the GUI starts this core with no console at all
/// (`CREATE_NO_WINDOW`), so stderr goes nowhere, and a person asked "what happened?" has to
/// have a file to open. stderr is kept beside it, because the integration tests read a core's
/// log there and a terminal is where a human starts one by hand.
fn init_logging(paths: &Paths) {
    let configured = std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|text| config::Config::from_str(&text).ok());
    let level = configured
        .as_ref()
        .map(|config| match config.log.level {
            config::LogLevel::Trace => "trace",
            config::LogLevel::Debug => "debug",
            config::LogLevel::Info => "info",
            config::LogLevel::Warn => "warn",
            config::LogLevel::Error => "error",
        })
        .unwrap_or("info");
    let (max_file_mb, max_files) = configured
        .as_ref()
        .map(|config| (config.log.max_file_mb, config.log.max_files))
        .unwrap_or((5, 5));
    let sink = logging::file_sink(&paths.logs_dir(), "lws-core", max_file_mb, max_files);
    let filter = tracing_subscriber::EnvFilter::try_from_env("LWS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    // Colour belongs to the destination: stderr is coloured only on a terminal, and the log
    // file never is (`logging::subscriber`). A hidden console on stderr used to colour the
    // file with it, which broke every parser that read the log.
    logging::init(
        filter,
        sink.clone().map(logging::file_writer),
        std::io::stderr().is_terminal(),
    );
    if let Some(sink) = sink {
        let path = sink.lock().map(|file| file.path().to_path_buf());
        if let Ok(path) = path {
            tracing::info!(path = %path.display(), "logging to the state directory");
        }
    }
}

/// `run`: the daemon.
fn run(args: &Args) -> Result<(), Failure> {
    init_logging(&args.paths);
    let options = RunOptions::parse(&args.rest)?;
    if options.dev_capture_injected {
        tracing::warn!("dev: capturing externally injected input");
    }
    let paths = &args.paths;

    // AC-16: the first run must not die on a missing `config.toml`. The owner of the
    // machine is not a developer, so the daemon writes a valid file itself — the
    // schema defaults plus this host's own LAN address — and only the case the
    // writer refuses (an unwritable directory) leaves the load error below standing.
    let wrote = match config::first_run::ensure_config(paths) {
        Ok(wrote) => wrote,
        Err(reason) => {
            tracing::warn!(%reason, "cannot write a first-run config.toml");
            false
        }
    };
    let config = config::Config::load(paths.config_file())?;
    if wrote {
        tracing::info!(
            path = %paths.config_file().display(),
            listen_address = %config.network.listen_address,
            "wrote the first-run configuration"
        );
    }
    // The probe runs *before* the runtime exists. On Linux `read_snapshot` reaches the
    // portals through zbus's blocking client, which drives its own runtime: called from
    // inside this one it panics («Cannot start a runtime from within a runtime») and core
    // never starts. `docs/03-architecture.md` puts the probe in the same breath as the
    // session decision, and that decision needs nothing from the runtime.
    let session = decide_session(&config);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))?;
    runtime
        .block_on(async move { run_async(paths, options, config, session).await })
        .map_err(Failure::Run)
}

/// The daemon's async half, so `run` only owns the runtime and the exit mapping.
async fn run_async(
    paths: &Paths,
    options: RunOptions,
    config: config::Config,
    session: LocalSession,
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
    let identity = Arc::new(identity);
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

    // `[clipboard]`: the second channel (ADR-008) exists only when the setting is on. It has a
    // listener of its own on `network.port_clipboard` and a dial list of its own, because
    // `KVM-042` («объём clipboard не блокирует key-up») is exactly about the two channels not
    // sharing a socket, a queue or a task.
    let (clipboard, clipboard_events) = if config.clipboard.enabled {
        let clipboard_listen = SocketAddr::new(listen.ip(), config.network.port_clipboard);
        let (endpoint, events) = clipboard::Clipboard::start(
            node_id,
            clipboard::Settings::from_config(&config.clipboard),
            Arc::clone(&identity),
            trust.clone(),
            clipboard_listen,
            QUEUE_CAPACITY,
        )
        .await?;
        (Some(endpoint), events)
    } else {
        // Nothing listens and nothing dials, and the `select!` arm is disabled by the
        // `clipboard.is_some()` guard, so this receiver never fires.
        let (_unused, events) = tokio::sync::mpsc::channel(1);
        (None, events)
    };

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
    let session_hello = local_hello.clone();
    // The pairing window is served by this node's own link listener, distinguished by ALPN
    // (`docs/05-protocol.md`: `pair/1` on 47901). The gate is the one switch the transport
    // reads; the session owns the window behind it.
    let pairing_gate = transport::PairingGate::closed();
    let manager_config = ManagerConfig {
        identity: Arc::clone(&identity),
        trust: trust.clone(),
        local_hello,
        listen,
        // The pairing window the listener serves by ALPN (`pair/1` on this same port):
        // closed until the user opens it, so no unknown peer reaches the listener before
        // then (`docs/05-protocol.md`, «Сопряжение»).
        pairing: pairing_gate.clone(),
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
    // `SequentialIds` is the replay/test source; production ids are random16
    // (`docs/05-protocol.md`), and a counter that restarts at 1 after a crash makes a peer
    // answer `LEASE_MISMATCH` because it remembers the burned ids.
    let ids = ids::RandomIds::new()?;
    let mut router = FocusRouter::new(node_id, timing, ids, session.fsm)
        .with_claim_on_startup(config.node.claim_on_startup)
        .with_require_emergency_hotkey(config.input.require_emergency_hotkey);
    if let Some(owner) = owner_pair(&config) {
        router = router.with_persisted_ownership(config.workspace.owner_claim_serial, Some(owner));
    }

    let endpoint =
        ctl::endpoint(private_root(paths)).map_err(|error| format!("control endpoint: {error}"))?;
    let server = ctl::Server::bind(&endpoint)
        .map_err(|error| format!("cannot own {}: {error}", endpoint.describe()))?;

    // The GUI's own endpoint, from the same rules (`ipc::transport::gui_endpoint`): it is
    // bound even with no window open, so `lws-gui` started first finds core when it appears.
    let gui_endpoint =
        ipc::transport::gui_endpoint(paths).map_err(|error| format!("GUI endpoint: {error}"))?;
    let gui_server = crate::gui::Server::bind(&gui_endpoint)
        .map_err(|error| format!("cannot own {}: {error}", gui_endpoint.describe()))?;
    tracing::info!(
        endpoint = gui_endpoint.describe().as_str(),
        "GUI channel bound"
    );
    let mut supervisor = Supervisor::new(
        options.agent.clone(),
        private_root(paths).to_path_buf(),
        private_root(paths).to_path_buf(),
        paths.logs_dir(),
        options.dry_inject,
        options.dry_input_events,
        options.dry_motion,
        options.dry_displays,
        options.dev_capture_injected,
        config.input.emergency_hotkey.clone(),
        backend_kind(&config),
        config.input.require_emergency_hotkey,
        config.timing.agent_core_silence_ms,
        // The portal tokens core ships to the agent: `portal-restore.toml`, which only
        // a Wayland session ever fills (`docs/03-architecture.md`).
        portal::PortalGrants::load(paths.portal_restore_file()),
        crate::agent::TakeoverSettings {
            detection: takeover_detection(&config),
            threshold_px: config.input.takeover_threshold_px,
        },
        repeat_mode(&config),
    );
    // The layout document is assembled by the session (`Session::rebuild_topology`), which
    // is also where it is rebuilt when the agent reports its displays; doing it here as well
    // would only produce a second, snapshot-only opinion of the same document.
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
        node_id,
        session_hello,
        session.backend,
        trust,
        // The layout document and the pointer gain: what edge-triggered transitions
        // resolve against (`docs/03-architecture.md`, «Топология и движение»).
        config.topology.clone(),
        config.input.pointer_gain,
        f64::from(config.input.edge_hysteresis_px),
        // The ownership pair belongs in the configuration file, so a restart resumes the
        // counter it learned instead of claiming serial 1 again (`ownership_store`).
        OwnershipStore::new(paths.config_file(), &config),
        LayoutStore::new(paths.config_file(), &config, node_id),
        clipboard,
        clipboard_events,
        gui_server,
        Arc::clone(&identity),
        paths.clone(),
        listen,
        config.node.display_name.clone(),
        config.input.emergency_hotkey.clone(),
        session.capabilities,
        options.dev_capture_injected,
        pairing_gate,
    );
    session.run().await.map_err(|error| error.to_string())
}

/// The `run`-only options.
struct RunOptions {
    dev_capture_injected: bool,
    dry_inject: bool,
    dry_input_events: usize,
    /// With `--dry-inject`: also walk the synthetic pointer over the right edge, so the
    /// edge-triggered transition can be exercised without a human at the keyboard.
    dry_motion: bool,
    /// With `--dry-inject`: report one synthetic monitor to exercise topology sharing.
    dry_displays: bool,
    agent: PathBuf,
    listen: Option<String>,
}

impl RunOptions {
    fn parse(rest: &[String]) -> Result<Self, Failure> {
        let mut dev_capture_injected = false;
        let mut dry_inject = false;
        let mut dry_input_events = 0usize;
        let mut dry_motion = false;
        let mut dry_displays = false;
        let mut agent = None;
        let mut listen = None;
        let mut index = 0;
        while index < rest.len() {
            let argument = rest[index].as_str();
            match argument {
                "--dev-capture-injected" => {
                    dev_capture_injected = true;
                    index += 1;
                }
                "--dry-inject" => {
                    dry_inject = true;
                    index += 1;
                }
                "--dry-motion" => {
                    dry_motion = true;
                    index += 1;
                }
                "--dry-displays" => {
                    dry_displays = true;
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
        if dry_displays && !dry_inject {
            return Err(Failure::Usage(
                "--dry-displays requires --dry-inject".to_owned(),
            ));
        }
        let agent = match agent {
            Some(agent) => agent,
            None => agent::default_agent_program().map_err(Failure::Run)?,
        };
        Ok(RunOptions {
            dev_capture_injected,
            dry_inject,
            dry_input_events,
            dry_motion,
            dry_displays,
            agent,
            listen,
        })
    }
}

/// The private directory the launch token lives in.
/// Creates the per-user directories a writing command needs.
///
/// Idempotent, and on Linux it also tightens the mode of an existing directory, which is
/// what makes a fresh `--root` usable without the caller preparing anything.
fn prepare_dirs(paths: &Paths) -> Result<(), Failure> {
    let mut dirs = vec![
        paths.config_dir().to_path_buf(),
        paths.data_dir().to_path_buf(),
        paths.state_dir().to_path_buf(),
        paths.logs_dir(),
    ];
    if let Some(runtime) = paths.runtime_dir() {
        dirs.push(runtime.to_path_buf());
    }
    for dir in dirs {
        identity::atomic::create_private_dir(&dir)
            .map_err(|error| Failure::Run(format!("cannot create {}: {error}", dir.display())))?;
    }
    Ok(())
}

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
            // `input.takeover_detection = "off"` turns the detector off, and a capability is
            // a promise about behaviour: without a detector the node must not claim
            // `RECEIVER_TAKEOVER` (AC-13). `evdev` keeps the native detector on Windows and
            // X11 and is refused on a portal receiver, which never claims it anyway.
            capabilities: platform_probe::Capabilities {
                receiver_takeover: session.capabilities.receiver_takeover
                    && config.input.takeover_detection != config::TakeoverDetection::Off,
                // ADR-008: `CAPABILITY_TEXT_CLIPBOARD` is claimed only when the clipboard is
                // enabled *and* this session has an adapter the build can drive. Windows and
                // X11 own the selection directly; the Wayland path is the clipboard portal,
                // which is not implemented here, so a Wayland node does not claim it.
                text_clipboard: config.clipboard.enabled
                    && matches!(
                        session.session_kind,
                        platform_probe::SessionKind::Windows | platform_probe::SessionKind::X11
                    ),
                ..session.capabilities
            },
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

/// `[input].takeover_detection` in IPC terms.
fn takeover_detection(config: &config::Config) -> protocol::ipc::TakeoverDetection {
    match config.input.takeover_detection {
        config::TakeoverDetection::Auto => protocol::ipc::TakeoverDetection::Auto,
        config::TakeoverDetection::Evdev => protocol::ipc::TakeoverDetection::Evdev,
        config::TakeoverDetection::Off => protocol::ipc::TakeoverDetection::Off,
    }
}

/// `[input].repeat_mode` in IPC terms (ADR-006: one engine, chosen here).
fn repeat_mode(config: &config::Config) -> protocol::ipc::RepeatMode {
    match config.input.repeat_mode {
        config::RepeatMode::ReceiverNative => protocol::ipc::RepeatMode::ReceiverNative,
        config::RepeatMode::ReceiverSoftware => protocol::ipc::RepeatMode::ReceiverSoftware,
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

#[cfg(test)]
mod run_options_tests {
    use super::RunOptions;

    #[test]
    fn dev_capture_injected_is_explicit_and_off_by_default() {
        let parse = |arguments: &[&str]| {
            let arguments = arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>();
            RunOptions::parse(&arguments).unwrap_or_else(|_| panic!("valid run options"))
        };
        assert!(!parse(&["--agent", "agent"]).dev_capture_injected);
        assert!(parse(&["--dev-capture-injected", "--agent", "agent"]).dev_capture_injected);
    }
}
