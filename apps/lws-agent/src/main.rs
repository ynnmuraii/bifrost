//! `lws-agent` — the input agent: capture, suppression, injection, the pressed
//! ledger and the watchdog. It has no network sockets (`docs/03-architecture.md`,
//! agent rule 1) and is spawned only by `lws-core`.
//!
//! # How core finds it
//!
//! Core creates the local IPC endpoint before spawning this process and passes the
//! endpoint in `--ipc-endpoint`; the 32-byte launch token goes through a private
//! file named by `--token-file`, never through `argv` or the environment
//! («не в argv и не в env (видны через `ps`/Process Explorer)»). The file is read
//! once and deleted. The agent connects, the first frame must be `AgentAttach`,
//! and the token in it is compared against the file.
//!
//! # Modes
//!
//! `--dry-inject` is the smoke/diagnostic mode: the backend is still selected and
//! reported truthfully, but no OS input path is touched — no capture hooks and no
//! injection; `Inject` commands are logged with their batch shape instead. It is
//! what lets the end-to-end transition be exercised without a human at the
//! keyboard (see `docs/work/e2-slice-procedure.md`).

mod backend;
mod displays;
mod events;
mod launch;
mod session;
mod takeover;

use std::path::PathBuf;
use std::process::ExitCode;

/// The framed-IPC version this agent speaks (`AgentFrame.major`/`minor`).
const FRAME_MAJOR: u32 = protocol::limits::PROTOCOL_MAJOR;
const FRAME_MINOR: u32 = protocol::limits::PROTOCOL_MINOR;

#[derive(Debug)]
struct Args {
    /// `\\.\pipe\…` name on Windows, socket path on Unix; core chose it.
    ipc_endpoint: String,
    /// File holding the 32 launch-token bytes; deleted on read.
    token_file: PathBuf,
    /// No OS capture hooks, no `SendInput`: `Inject` is logged instead.
    dry_inject: bool,
    /// Test aid: emit this many synthetic key edges after `BeginCapture(REMOTE)`.
    dry_input_events: usize,
    /// Test aid: walk the synthetic pointer across this node's right edge and back.
    ///
    /// The edge trigger needs a pointer that *moves* in `Local`, which no OS call is
    /// involved in for a dry session, so this emits a fixed pattern in the node's logical
    /// space: right from x = 1600 to 1960 (crossing a 1920-wide display at the right
    /// edge), a one-second pause, then left back to 1200 and a two-second pause. It needs
    /// a display that covers those coordinates — the smoke uses the
    /// `[[topology.displays]]` snapshot, which is exactly what core falls back to when
    /// the agent reports nothing.
    dry_motion: bool,
}

const USAGE: &str = "\
lws-agent — LAN Workspace input agent (spawned by lws-core)

USAGE:
    lws-agent --ipc-endpoint <name|path> --token-file <path> [--dry-inject]
              [--dry-input-events <n>] [--help]

OPTIONS:
    --ipc-endpoint <spec>   local IPC endpoint core created for this launch
    --token-file <path>     file with the 32-byte launch token (deleted on read)
    --dry-inject            no capture hooks and no injection; log Inject instead
    --dry-input-events <n>  with --dry-inject: emit n synthetic key edges (smoke aid)
    --dry-motion            with --dry-inject: walk the pointer over the right edge and back
    --help                  print this help
";

fn parse_args() -> Result<Args, String> {
    let mut ipc_endpoint = None;
    let mut token_file = None;
    let mut dry_inject = false;
    let mut dry_input_events = 0usize;
    let mut dry_motion = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--ipc-endpoint" => {
                ipc_endpoint = Some(args.next().ok_or("--ipc-endpoint needs a value")?);
            }
            "--token-file" => {
                token_file = Some(PathBuf::from(
                    args.next().ok_or("--token-file needs a value")?,
                ));
            }
            "--dry-inject" => dry_inject = true,
            "--dry-motion" => dry_motion = true,
            "--dry-input-events" => {
                let value = args.next().ok_or("--dry-input-events needs a value")?;
                dry_input_events = value
                    .parse()
                    .map_err(|_| format!("--dry-input-events: {value} is not a number"))?;
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }

    Ok(Args {
        ipc_endpoint: ipc_endpoint.ok_or("--ipc-endpoint is required")?,
        token_file: token_file.ok_or("--token-file is required")?,
        dry_inject,
        dry_input_events,
        dry_motion,
    })
}

fn init_logging() {
    // stderr only: stdout stays free for any future stdio channel, and the logs of
    // this process are never the place for key usages («Никогда: нажатия клавиш»).
    let filter = tracing_subscriber::EnvFilter::try_from_env("LWS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn main() -> ExitCode {
    init_logging();
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("lws-agent: {message}");
            return ExitCode::from(2);
        }
    };
    tracing::info!(
        endpoint = args.ipc_endpoint.as_str(),
        dry_inject = args.dry_inject,
        "agent starting"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("lws-agent: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(session::run(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "agent stopped");
            ExitCode::FAILURE
        }
    }
}
