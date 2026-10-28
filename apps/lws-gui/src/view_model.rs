//! Everything the window decides, and nothing about how it looks.
//!
//! The split this crate is built around (see the crate docs): [the view model](ViewModel)
//! is plain data with no `egui` type anywhere near it, [the UI](crate::ui) turns it
//! into widgets and reports what the user asked for as [`Action`]s, and [`Bridge`] is the only
//! place that connects the two. That is what makes `cargo test -p lws-gui` runnable on a host
//! with no display: a [`ViewModel`] is a struct literal, and `apply` is a pure fold.
//!
//! # The bridge rule
//!
//! The UI never blocks on the network (`docs/03-architecture.md`: «GUI не участвует в
//! горячем пути»). Every request is fire-and-forget with a `request_id`; the answer comes
//! back as a frame and is folded by [`ViewModel::apply`], and a request that stays
//! unanswered for [`REQUEST_TIMEOUT`] is what makes the header say «Служба не отвечает».
//! The async half is [`GuiClient`], which owns one connection task on its own thread and
//! talks to the UI over an unbounded channel the app drains with `try_recv`.
//!
//! # What the window asks the core for
//!
/// [`Action`] is the whole vocabulary: the wire requests of `ipc.proto`
/// (`GetStatus`, `ClaimOwnership`, `StartPairing`, …), the one thing the client does by
/// itself (`StartCore`), and the local edits the window applies to its own
/// state (`ToggleModal`, `MoveNode`, …). Only the first group is a `GuiFrame`;
/// the app sends those to the client and folds the rest into the view model, so no decision
/// ever happens in drawing code. `StopCore` asks the core to stop itself, so it also works for
/// a service started elsewhere; only a timed-out request may use the pid this window started.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ::ipc::transport::Endpoint;
use config::Paths;
// `protocol::ipc` and the `ipc` crate are two different things, and this module needs both:
// `ipc::` is the wire vocabulary, and the local transport is named with a leading `::`.
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;
use ui_text::{fill, fill_named, text, Key, Lang};

pub mod hotkeys;
pub mod layout;

/// How long a request may stay unanswered before the window says «Служба не отвечает».
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a toast stays when nobody holds it.
pub const TOAST_FOR: Duration = Duration::from_secs(8);

/// How long «Сохранено» stays.
pub const SAVED_FOR: Duration = Duration::from_millis(1500);

/// How long a hint stays.
pub const HINT_FOR: Duration = Duration::from_secs(4);

/// How many lines of the shared log the window keeps in front of the user.
pub const LOG_LINES: usize = 20;

/// How long the client waits before it tries the core again.
///
/// The build spec asks for a 2 s retry, so a core started by hand (or by «Запустить» on
/// another desktop) makes the window come alive on its own.
pub const RETRY_DELAY: Duration = Duration::from_secs(2);

/// How long the GUI waits for the core to stop itself before using its owned pid or reporting failure.
pub const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long «Запустить» waits for a core to answer before it calls the start failed.
///
/// Starting a core is launching a process and waiting for its endpoint; a core that has not
/// answered by then is not going to.
pub const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(15);

/// The oldest requests kept while they wait for an answer.
const INFLIGHT_LIMIT: usize = 32;

// Coordinator heartbeats are bookkeeping, not user operations or inflight requests.
const COORDINATOR_REQUEST_ID: u64 = u64::MAX;

// ---------------------------------------------------------------- small types

/// A window over the map: the rail's service sections and Appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modal {
    /// «Пары».
    Pairs,
    /// «Клавиши».
    Keys,
    /// «Буфер обмена».
    Clip,
    /// «Диагностика».
    Diag,
    /// «Настройки».
    Settings,
    /// «Внешний вид», rendered from the appearance rail's open flag.
    Appearance,
}

impl Modal {
    /// The five service sections, in rail order; Appearance uses its dedicated open flag.
    pub const ALL: [Modal; 5] = [
        Modal::Pairs,
        Modal::Keys,
        Modal::Clip,
        Modal::Diag,
        Modal::Settings,
    ];

    /// The two-digit number of the section.
    pub const fn num(self) -> &'static str {
        match self {
            Modal::Pairs => "01",
            Modal::Keys => "02",
            Modal::Clip => "03",
            Modal::Diag => "04",
            Modal::Settings => "05",
            Modal::Appearance => "06",
        }
    }

    /// The title of the section.
    pub fn title(self, lang: Lang) -> &'static str {
        match self {
            Modal::Pairs => text(lang, Key::Pairs),
            Modal::Keys => text(lang, Key::Keys),
            Modal::Clip => text(lang, Key::Clipboard),
            Modal::Diag => text(lang, Key::Diagnostics),
            Modal::Settings => text(lang, Key::Settings),
            Modal::Appearance => text(lang, Key::Appearance2),
        }
    }

    /// The «command» of the terminal-style header line.
    pub const fn command(self) -> &'static str {
        match self {
            Modal::Pairs => "bifrost peers",
            Modal::Keys => "bifrost keys",
            Modal::Clip => "bifrost clip",
            Modal::Diag => "bifrost diag",
            Modal::Settings => "bifrost config",
            Modal::Appearance => "bifrost theme",
        }
    }

    /// The caption after the progress bar.
    pub fn subtitle(self, lang: Lang) -> &'static str {
        match self {
            Modal::Pairs => text(lang, Key::WhoThisComputerTrusts),
            Modal::Keys => text(lang, Key::TheInputServiceSHotkeys),
            Modal::Clip => text(lang, Key::CopyHerePasteThere),
            Modal::Diag => text(lang, Key::AReportForTroubleshooting),
            Modal::Settings => text(lang, Key::AddressAndService),
            Modal::Appearance => text(lang, Key::ThemeAndBackground),
        }
    }
}

/// Which side of this computer the other one is placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// The other computer is to the left.
    Left,
    /// The other computer is to the right.
    #[default]
    Right,
    /// The other computer is above.
    Top,
    /// The other computer is below.
    Bottom,
}

impl Side {
    /// Every map direction, horizontal first and then vertical.
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    /// The direction's human-readable label.
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Side::Left => text(lang, Key::Left),
            Side::Right => text(lang, Key::Right),
            Side::Top => text(lang, Key::Top),
            Side::Bottom => text(lang, Key::Bottom),
        }
    }

    /// The edge of *this* computer the pointer leaves through, and the edge it enters the
    /// other one at.
    pub const fn edges(self) -> (config::Edge, config::Edge) {
        match self {
            Side::Left => (config::Edge::Left, config::Edge::Right),
            Side::Right => (config::Edge::Right, config::Edge::Left),
            Side::Top => (config::Edge::Top, config::Edge::Bottom),
            Side::Bottom => (config::Edge::Bottom, config::Edge::Top),
        }
    }

    /// `true` when the two computers are side by side (the shared axis is vertical).
    pub const fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// Where the local IPC endpoint of the core is, or why it cannot be resolved.
///
/// `ipc::transport::gui_endpoint` fails on Linux with `XDG_RUNTIME_DIR` unset, which the
/// window has to show instead of a raw path nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEndpoint {
    /// The documented endpoint of this user.
    Ready {
        /// `Endpoint::describe()`, for the screen and for error messages.
        description: String,
    },
    /// The endpoint could not be built; the text is what the window shows.
    Failed {
        /// The reason `gui_endpoint` gave.
        why: String,
    },
}

impl CoreEndpoint {
    /// The text the status screen shows about the endpoint.
    pub fn description(&self) -> &str {
        match self {
            CoreEndpoint::Ready { description } => description,
            CoreEndpoint::Failed { why } => why,
        }
    }
}

/// The link to the core, as the window sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// No core is answering; `why` is the last error the client reported.
    Disconnected {
        /// What went wrong, in the core's own words.
        why: String,
    },
    /// The local channel carries `GuiFrame`s.
    Connected,
}

impl Connection {
    /// `true` while the window has a live link to the core.
    pub const fn is_connected(&self) -> bool {
        matches!(self, Connection::Connected)
    }
}

/// One request the window may have in flight.
///
/// The answer to a request is an `OpResult`, which carries the `request_id` and a code but
/// no memory of what was asked, so the window keeps this to turn `PAIRING_WINDOW_OPEN` into
/// a sentence about pairing rather than a sentence about layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// `GetStatus`.
    GetStatus,
    /// `GetTopology`.
    GetTopology,
    /// `ClaimOwnership`.
    Claim,
    /// `AbandonOwnership`.
    Abandon,
    /// `StartPairing`.
    StartPairing,
    /// `PairWith`: success acknowledges the dial reaching a prompt, not a completed pair.
    PairWith,
    PairingRequest,
    SetPairingRequests,
    /// `PairingAnswer`.
    PairingAnswer,
    /// `UpdateTopology`.
    UpdateTopology,
    /// Update the workspace, peers first.
    UpdateAll,
    /// Allow or refuse remote update requests.
    SetRemoteUpdate,
    /// Register the GUI process that coordinates worker updates.
    RegisterUpdateCoordinator,
    /// `SetClipboard`.
    SetClipboard,
    /// Persist LAN discovery through the core.
    SetDiscovery,
    SetGameGuard,
    /// Read the in-memory clipboard history.
    GetClipboardHistory,
    /// Copy a history item through the core's local clipboard adapter.
    CopyClipboardHistory,
    /// Send a history item to one online member.
    SendClipboardHistory,
    /// Persist clipboard routing policy.
    SetClipboardPolicy,
    /// Clear the core's in-memory clipboard history.
    ClearClipboardHistory,
    /// `SetAppearance`.
    SetAppearance,
    /// `SetHotkey`.
    SetHotkey,
    /// `RevokePeer`.
    RevokePeer,
    /// `ExportDiagnostics`.
    ExportDiagnostics,
    /// `ResetPortalGrants`.
    ResetPortalGrants,
    /// `RetryAgent`.
    RetryAgent,
    /// `Shutdown`.
    Shutdown,
}

impl Request {
    /// The request's name in a log line.
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Request::GetStatus => text(lang, Key::State),
            Request::GetTopology => text(lang, Key::Layout),
            Request::Claim => text(lang, Key::TakeControl),
            Request::Abandon => text(lang, Key::GiveControlBack),
            Request::StartPairing => text(lang, Key::OpenThePairingWindow),
            Request::PairWith => text(lang, Key::ConnectByAddress),
            Request::PairingRequest => text(lang, Key::PairingRequestIncoming),
            Request::SetPairingRequests => text(lang, Key::PairingRequests),
            Request::PairingAnswer => text(lang, Key::AnswerThePairing),
            Request::UpdateTopology => text(lang, Key::SaveTheLayout),
            Request::UpdateAll => text(lang, Key::UpdateAllComputers),
            Request::SetRemoteUpdate => text(lang, Key::AllowRemoteUpdates),
            Request::RegisterUpdateCoordinator => text(lang, Key::State),
            Request::SetClipboard => text(lang, Key::TheClipboard),
            Request::SetDiscovery => text(lang, Key::LanDiscovery),
            Request::SetGameGuard => text(lang, Key::GameMode),
            Request::GetClipboardHistory => text(lang, Key::TheClipboardHistory),
            Request::CopyClipboardHistory => text(lang, Key::CopyFromTheHistory),
            Request::SendClipboardHistory => text(lang, Key::SendClipboardTo),
            Request::SetClipboardPolicy => text(lang, Key::ClipboardRouting),
            Request::ClearClipboardHistory => text(lang, Key::ClearTheClipboardHistory),
            Request::SetAppearance => text(lang, Key::TheEdgeEffects),
            Request::SetHotkey => text(lang, Key::KeyboardShortcut),
            Request::RevokePeer => text(lang, Key::RemoveAComputerFromThePair),
            Request::ExportDiagnostics => text(lang, Key::SaveTheDiagnostics),
            Request::ResetPortalGrants => text(lang, Key::ForgetThePortalGrants),
            Request::RetryAgent => text(lang, Key::StartTheInputServiceAgain),
            Request::Shutdown => text(lang, Key::StopTheService),
        }
    }
}

/// The core process this window started, when it started one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedCore {
    /// The program that was started.
    pub program: PathBuf,
    /// Its process id, when the platform gave one.
    pub pid: Option<u32>,
}

/// A finished operation, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `true` for «Готово», `false` for «Не получилось».
    pub ok: bool,
    /// The sentence, already in Russian.
    pub text: String,
}

impl Outcome {
    /// A success.
    pub fn done(text: impl Into<String>) -> Self {
        Outcome {
            ok: true,
            text: text.into(),
        }
    }

    /// A failure.
    pub fn failed(text: impl Into<String>) -> Self {
        Outcome {
            ok: false,
            text: text.into(),
        }
    }

    /// The line as the screens draw it.
    pub fn line(&self, lang: Lang) -> String {
        if self.ok {
            fill(lang, Key::Done, &[&self.text])
        } else {
            fill(lang, Key::Failed, &[&self.text])
        }
    }
}

// ---------------------------------------------------------------- local config

/// The settings the window shows, read from `config.toml`.
///
/// Reading the file is what §5.4 of the build spec asks for and the only reason the GUI
/// opens it. Service-owned settings, including hotkeys, come from IPC status instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    /// `[node].display_name`.
    pub display_name: String,
    /// `[network].port_input` — the port the other computer dials.
    pub port_input: u16,
    /// `[network].listen_address` — «Адрес этого компьютера в локальной сети».
    pub listen_address: String,
    /// `[discovery].enabled`, until the core publishes its authoritative status.
    pub discovery_enabled: bool,
    /// `[pairing].accept_requests`, until core publishes authoritative status.
    pub accept_pairing_requests: bool,
    /// `Paths::config_file()`, so a user can be told which file is being shown.
    pub config_file: PathBuf,
    /// `Paths::state_dir()`, where `ExportDiagnostics` writes.
    pub state_dir: PathBuf,
    /// The directories the window resolved, when it resolved them.
    ///
    /// «Применить» on the address writes through them, so this is the one place the window
    /// needs more than the display strings above; `None` for a host whose directories could
    /// not be resolved, where the write is refused in words instead of guessed.
    pub paths: Option<Paths>,
    /// `true` when `config.toml` was not there when the window read it.
    ///
    /// The core creates the file on its first run, so the window that read nothing comes
    /// back to it once — see [`ViewModel::on_core_started`].
    pub config_was_missing: bool,
    /// The reason the file was not used, when it was not.
    pub note: Option<String>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        LocalConfig {
            display_name: "Workstation".to_owned(),
            port_input: 47901,
            listen_address: String::new(),
            discovery_enabled: true,
            accept_pairing_requests: true,
            config_file: PathBuf::new(),
            state_dir: PathBuf::new(),
            paths: None,
            config_was_missing: false,
            note: None,
        }
    }
}

impl LocalConfig {
    /// Reads `config.toml` through [`Paths`]; every failure is a note, never an error.
    pub fn load(paths: &Paths) -> Self {
        let lang = Lang::from_system();
        let file = paths.config_file();
        let mut config = LocalConfig {
            config_file: file.clone(),
            state_dir: paths.state_dir().to_path_buf(),
            paths: Some(paths.clone()),
            ..LocalConfig::default()
        };
        config.config_was_missing = !file.is_file();
        match config::Config::load(&file) {
            Ok(loaded) => {
                config.display_name = loaded.node.display_name.clone();
                config.port_input = loaded.network.port_input;
                config.listen_address = loaded.network.listen_address.clone();
                config.discovery_enabled = loaded.discovery.enabled;
                config.accept_pairing_requests = loaded.pairing.accept_requests;
            }
            Err(error) => {
                config.note = Some(fill(
                    lang,
                    Key::TheSettingsWereNotReadTheDefaultsAreShown,
                    &[&crate::view_model::short_error(&error.to_string())],
                ));
            }
        }
        config
    }
}

/// The first line of an error, for a sentence that has to fit next to a widget.
pub fn short_error(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        line.chars().take(120).collect::<String>() + "…"
    } else {
        line.to_owned()
    }
}

// ---------------------------------------------------------------- the editor model

/// One display as the editor draws and moves it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorDisplay {
    /// The display's stable id.
    pub id: String,
    /// Logical x inside the owning node's own space.
    pub x: f64,
    /// Logical y inside the owning node's own space.
    pub y: f64,
    /// Logical width.
    pub w: f64,
    /// Logical height.
    pub h: f64,
    /// `true` for the node's own origin.
    pub primary: bool,
    /// `false` on Wayland portals: the size is a good guess, not a measurement.
    pub native_known: bool,
    /// Current-mode pixels and scale, independent of layout geometry.
    pub native_w: f64,
    pub native_h: f64,
    pub scale: f64,
    /// Rotation in degrees (0, 90, 180, 270).
    pub rotation: u32,
}

impl EditorDisplay {
    /// The display as the wire carries it.
    fn from_view(view: &ipc::DisplayView) -> Self {
        EditorDisplay {
            id: view.id.clone(),
            x: finite(view.logical_x, 0.0),
            y: finite(view.logical_y, 0.0),
            w: finite(view.logical_w, 0.0),
            h: finite(view.logical_h, 0.0),
            primary: view.primary,
            native_known: view.native_known,
            rotation: view.rotation_degrees,
            native_w: f64::from(view.native_width),
            native_h: f64::from(view.native_height),
            scale: finite(view.scale, 1.0).max(f64::EPSILON),
        }
    }
}

/// One member of the workspace, as the editor draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorNode {
    /// The node id, lower-case hex — the form both the wire and `[topology]` use.
    pub node_id: String,
    /// The display name, empty when the core knew none.
    pub display_name: String,
    /// Last known sanitized OS name/version; empty for old peers.
    pub system_name: String,
    /// Advisory, as `NodeDisplays.online`.
    pub online: bool,
    /// The member's displays.
    pub displays: Vec<EditorDisplay>,
}

impl EditorNode {
    /// The name to show; a node without one is named by its node id.
    pub fn label(&self, lang: Lang) -> String {
        if self.display_name.is_empty() {
            fill(lang, Key::ComputerNamed, &[&short_id(&self.node_id)])
        } else {
            self.display_name.clone()
        }
    }
}

/// The layout editor's model: the applied document, plus what the user has done to it.
///
/// The document is edited as a whole (ADR-013 rejected merging rows), so the model is the
/// applied [`ipc::TopologyDoc`] turned into something draggable, plus the placement the user
/// chose and the link they are building. [`ViewModel::layout_toml`] serialises it back into
/// the `[topology]` schema, which is what `UpdateTopology` carries.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayoutEditor {
    /// The applied revision the editor started from.
    pub revision: u64,
    /// The author of that revision, lower-case hex; empty when there is none.
    pub author: String,
    /// `Status.layout_editable`: false when no coordinator is reachable.
    pub editable: bool,
    /// The members, in the order the core listed them.
    pub nodes: Vec<EditorNode>,
    /// The applied links, still in wire form: they are rewritten as `[topology]` rows.
    pub links: Vec<ipc::LinkView>,
    /// Explicit shared map exclusions, retained across GUI edits.
    pub forgotten_nodes: Vec<String>,
    /// The drawn shape of every member, parallel to `nodes`.
    pub shapes: Vec<crate::map::Shape>,
    /// Where the user has put every member on the stage, parallel to `nodes`.
    pub placement: Vec<crate::map::Pt>,
    /// Where the applied document puts them: what `placement` is compared with.
    pub baseline: Vec<crate::map::Pt>,
    /// Independently adjusted endpoint draft, in stable display-local coordinates.
    pub paddles: Option<Vec<config::LinkConfig>>,
    /// Stage units per logical unit the shapes were drawn at.
    pub scale: f64,
    /// The automatic save of the map: what is wanted, what is in flight, what is owed.
    pub save: Save,
}

/// The automatic save of the map (`layout.rs`): a drop is the only thing that starts one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Save {
    /// The links of the latest drop the core has not applied yet: latest wins.
    pub wanted: Option<Vec<config::LinkConfig>>,
    /// Exact placement of the last valid drop, for interrupted/invalid drag rollback.
    pub placement: Option<Vec<crate::map::Pt>>,
    /// Endpoint draft of the last accepted drop, including while its acknowledgment is pending.
    pub paddles: Option<Vec<config::LinkConfig>>,
    /// The one `UpdateTopology` in flight.
    pub sent: Option<Sent>,
    /// The retry after a `LAYOUT_CONFLICT` of this draft was spent.
    pub retried: bool,
    /// A `GetTopology` after a conflict is on its way; the draft goes on top of it.
    pub reloading: bool,
    /// A newer draft than the saved one waits for a revision past this one.
    pub awaiting: Option<u64>,
}

/// The `UpdateTopology` in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    /// Its `request_id`.
    pub request_id: u64,
    /// The revision it was edited from.
    pub base_revision: u64,
    /// The links it carried.
    pub links: Vec<config::LinkConfig>,
}

/// A quiet one-line note on the map («Сохранено», a hint) until it runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    /// The words.
    pub text: String,
    /// When it goes.
    pub until: Instant,
}

impl LayoutEditor {
    /// `true` once a document has been received.
    pub fn has_document(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// `true` while the map differs from the applied document (the «черновик»).
    pub fn dirty(&self) -> bool {
        self.placement != self.baseline || self.paddles.is_some()
    }
}

// ---------------------------------------------------------------- view model

/// One request waiting for its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    /// The `request_id` the frame carried.
    pub id: u64,
    /// What was asked.
    pub request: Request,
    /// When the window stops believing in an answer.
    pub deadline: Instant,
    /// The destination name at dispatch, retained even if membership changes.
    pub clipboard_target: Option<String>,
}

/// The pairing screen's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pairing {
    /// The last `PairingWindow` the core pushed (`open: false` clears it).
    pub window: Option<ipc::PairingWindow>,
    /// The six-digit confirmation of ADR-010, waiting for an answer.
    pub prompt: Option<ipc::PairingPrompt>,
    /// The address field of «Подключиться по адресу».
    pub address: String,
    /// `true` once «Подробнее» was opened (ADR-010: the full fingerprints live there).
    pub details: bool,
    /// When the open window runs out, by this window's clock (the core pushes the seconds
    /// left only when the window opens, so the countdown is counted here).
    pub window_deadline: Option<Instant>,
    /// The computer that was just paired, until «Готово».
    pub paired: Option<String>,
    /// `true` from «Подключиться» until the core answers or shows the code.
    pub dialing: bool,
}

impl Pairing {
    /// The code, with its digits grouped as «482 913».
    pub fn code(&self) -> Option<String> {
        self.prompt
            .as_ref()
            .map(|prompt| spaced_code(&prompt.sas_code))
    }

    /// The attempt every answer of this prompt must name.
    pub fn attempt_id(&self) -> Option<Vec<u8>> {
        self.prompt
            .as_ref()
            .map(|prompt| prompt.attempt_id.clone())
            .filter(|attempt| !attempt.is_empty())
    }
}

/// The settings screen's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// What a clipboard toggle asked for, until `Status` shows it or the core refuses.
    pub clipboard_pending: Option<bool>,
    /// Requested discovery state until the core publishes it or refuses.
    pub discovery_pending: Option<bool>,
    pub game_guard_apps_open: bool,
    /// The member «Удалить компьютер из пары» is aimed at.
    pub revoke: Option<String>,
    /// What `ExportDiagnostics` answered.
    pub export: Option<Outcome>,
    /// «Адрес в локальной сети»: what the file names, and what may be picked.
    pub lan: Lan,
    /// The service controls, including graceful stop for cores started elsewhere.
    pub service: Service,
    /// `true` while «Настройки созданы автоматически» waits for «Понятно».
    pub first_run: bool,
    /// `true` once that notice was raised: never twice in one session.
    pub first_run_reported: bool,
    /// Whether this user's login starts the current executable.
    pub autostart: bool,
    /// A failed change to the system login entry.
    pub autostart_error: Option<String>,
}

/// «Адрес этого компьютера в локальной сети»: the address the file names, and the ones
/// that may be picked instead of it.
///
/// The current value lives in [`LocalConfig::listen_address`] — one copy of the truth, read
/// from `config.toml` and written back by «Применить» — so this is only the part of the
/// block that is not the file's own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lan {
    /// The addresses this computer has, best first (`first_run::lan_addresses`).
    pub candidates: Vec<String>,
    /// The address picked in the dropdown; `None` until the user picks one.
    pub choice: Option<String>,
    /// What «Применить» answered, in words.
    pub applied: Option<Outcome>,
}

impl Lan {
    /// Addresses the computer has plus a valid address named in its config. Loopback is
    /// never a LAN address, even when an older config still names it.
    pub fn options(&self, current: &str) -> Vec<String> {
        let mut options: Vec<String> = self
            .candidates
            .iter()
            .filter(|address| is_lan_address(address))
            .cloned()
            .collect();
        if is_lan_address(current) && !options.iter().any(|option| option == current) {
            options.push(current.to_owned());
        }
        options
    }

    /// The picked address, or the valid configured address, or a replacement for a bad one.
    pub fn selected(&self, current: &str) -> Option<String> {
        let options = self.options(current);
        if let Some(choice) = self
            .choice
            .as_ref()
            .filter(|choice| options.contains(choice))
        {
            return Some(choice.clone());
        }
        if options.iter().any(|option| option == current) {
            return Some(current.to_owned());
        }
        (!current.is_empty())
            .then(|| options.into_iter().next())
            .flatten()
    }
}

/// Only a parsed, non-loopback IPv4 address can be offered as a LAN address.
fn is_lan_address(address: &str) -> bool {
    address
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| !address.is_loopback())
}

/// The service, and the last word the window had about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Service {
    /// What «Перезапустить службу» is waiting for, while it waits.
    pub restart: Option<Restart>,
    /// What «Запустить» on the «служба не запущена» banner is waiting for.
    pub start: ServiceStart,
    /// When «Остановить службу» was sent through the core's graceful shutdown request.
    pub stopping: Option<Instant>,
    /// What the last service operation answered, in words.
    pub line: Option<Outcome>,
}

/// What «Перезапустить службу» is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restart {
    /// Graceful shutdown was sent; process/listener exit still needs to be observed.
    Stopping {
        /// When the stop was sent, so the wait has an end.
        since: Instant,
    },
    /// The fresh core was launched; the link is on its way back.
    Starting,
}

/// What «Перезапустить службу» is to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStep {
    /// Send graceful Shutdown IPC, then wait for process/listener exit.
    Stop,
    /// No active connection: still check listener ownership before launching.
    Start,
    /// The window must not act; why is already in [`Service::line`].
    Refused,
}

/// The one button of the «служба не запущена» banner, and the wait behind it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServiceStart {
    /// Nothing was asked for: the banner offers «Запустить» and is still.
    #[default]
    Idle,
    /// A core was asked for and has not answered: the banner shows the wait and its one
    /// button is drawn disabled.
    Starting {
        /// When the start was asked for, so the wait has an end.
        since: Instant,
    },
    /// The last start did not bring a core up: the banner says why and offers «Ещё раз».
    Failed {
        /// One line saying what went wrong.
        reason: String,
    },
}

impl ServiceStart {
    /// `true` while a start is in flight.
    pub fn is_starting(&self) -> bool {
        matches!(self, Self::Starting { .. })
    }

    /// The label of the banner's single button: the failed start is retried, not re-labelled.
    pub fn button(&self) -> Key {
        match self {
            Self::Failed { .. } => Key::OnceMore,
            Self::Idle | Self::Starting { .. } => Key::Start,
        }
    }

    /// The line the banner says under itself, when the last start failed.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed { reason } => Some(reason),
            Self::Idle | Self::Starting { .. } => None,
        }
    }
}

/// Everything the screens draw, and every decision behind it.
#[derive(Debug, Clone)]
pub struct ViewModel {
    /// The endpoint the core serves, or the reason there is none.
    pub endpoint: CoreEndpoint,
    /// The state of the local channel.
    pub connection: Connection,
    /// The core process this window started, when it started one.
    pub core: Option<StartedCore>,
    /// The settings read from `config.toml`.
    pub config: LocalConfig,
    /// The window over the map, when one is open.
    pub modal: Option<Modal>,
    /// The look of the window, kept in the GUI's own settings file.
    pub look: crate::gui_settings::Look,
    /// The last `Status`, kept across a disconnect so the window is not blank.
    pub status: Option<ipc::Status>,
    /// The latest shortcut validation or portal-registration feedback.
    pub hotkeys_error: Option<String>,
    /// The editor's model.
    pub editor: LayoutEditor,
    /// The pairing screen's state.
    pub pairing: Pairing,
    pub incoming_pair_request: Option<ipc::IncomingPairingRequest>,
    pub outgoing_pair_request: Option<ipc::OutgoingPairingRequest>,
    /// The settings screen's state.
    pub settings: Settings,
    /// GUI-only updater state, also used by offscreen screenshots.
    pub update: crate::updater::State,
    pub update_notice: bool,
    /// Remote unpair notice awaiting dismissal; repeated status pushes do not reopen it.
    pub unpair_notice: Option<String>,
    unpair_notice_seen: u64,
    pub update_confirm: bool,
    pub update_all_confirm: bool,
    pub checked_release: Option<crate::updater::Release>,
    /// Sanitized versions from topology and live status, keyed by node id.
    pub app_versions: std::collections::BTreeMap<String, String>,
    /// Metadata only; full clipboard text remains in the core.
    pub clipboard_history: Vec<ipc::ClipboardHistoryEntry>,
    /// The last finished operation, shown as a toast until [`TOAST_FOR`] runs out.
    pub outcome: Option<Outcome>,
    /// The shared Appearance modal is open; `modal` is empty while this flag is set.
    pub appearance_open: bool,
    /// A quiet note on the map.
    pub flash: Option<Flash>,
    /// The toast the timer below belongs to.
    outcome_seen: Option<Outcome>,
    /// When the toast was first drawn; `None` while it waits to be.
    outcome_since: Option<Instant>,
    /// The pointer is on the toast: its timer is paused.
    outcome_hold: bool,
    outcome_hold_since: Option<Instant>,
    /// Requests the view model itself made (the automatic save, a status re-read): already
    /// recorded as in flight, to be put on the wire by [`Bridge`].
    outgoing: Vec<(u64, Msg)>,
    /// The last [`LOG_LINES`] lines of what happened.
    pub log: VecDeque<String>,
    /// The requests waiting for an answer.
    pub inflight: Vec<Inflight>,
    /// `true` while a request is past its [`REQUEST_TIMEOUT`].
    pub silent: bool,
    /// The next `request_id` (a local counter: the core only echoes it).
    next_request_id: u64,
}

impl ViewModel {
    pub fn node_app_version(&self, node_id: &str) -> String {
        if self.self_node() == Some(node_id) {
            protocol::sanitize_app_version(protocol::app_version())
        } else {
            self.app_versions.get(node_id).cloned().unwrap_or_default()
        }
    }

    pub fn highest_app_version(&self) -> String {
        let release = self.checked_release.as_ref().or({
            if let crate::updater::State::Available(release) = &self.update {
                Some(release)
            } else {
                None
            }
        });
        self.editor
            .nodes
            .iter()
            .filter_map(|node| self.app_versions.get(&node.node_id).map(String::as_str))
            .chain(std::iter::once(protocol::app_version()))
            .chain(release.map(|release| release.tag_name.trim_start_matches('v')))
            .filter_map(|version| semver::Version::parse(version).ok())
            .max_by(|a, b| a.cmp_precedence(b))
            .map(|version| version.to_string())
            .unwrap_or_default()
    }

    pub fn node_needs_update(&self, node_id: &str) -> bool {
        version_is_older(&self.node_app_version(node_id), &self.highest_app_version())
    }

    /// A window that knows where the core is and nothing else.
    pub fn new(endpoint: CoreEndpoint, config: LocalConfig) -> Self {
        let lang = Lang::from_system();
        let mut view = ViewModel {
            endpoint,
            connection: Connection::Disconnected {
                why: text(lang, Key::TheServiceHasNotAnsweredYet).to_owned(),
            },
            core: None,
            config,
            modal: None,
            look: crate::gui_settings::Look::default(),
            status: None,
            hotkeys_error: None,
            editor: LayoutEditor::default(),
            pairing: Pairing::default(),
            incoming_pair_request: None,
            outgoing_pair_request: None,
            settings: Settings::default(),
            update: crate::updater::State::Idle,
            update_notice: false,
            unpair_notice: None,
            unpair_notice_seen: 0,
            update_confirm: false,
            update_all_confirm: false,
            checked_release: None,
            app_versions: Default::default(),
            clipboard_history: Vec::new(),
            outcome: None,
            appearance_open: false,
            flash: None,
            outcome_seen: None,
            outcome_since: None,
            outcome_hold: false,
            outcome_hold_since: None,
            outgoing: Vec::new(),
            log: VecDeque::new(),
            inflight: Vec::new(),
            silent: false,
            next_request_id: 0,
        };
        view.push_log("Окно открыто");
        view
    }

    // ------------------------------------------------------------ the connection

    /// The client reached the core: greeting frames are on their way.
    pub fn on_connected(&mut self, endpoint: &str) {
        self.reload_first_run_config();
        self.connection = Connection::Connected;
        self.silent = false;
        // A core that answers is a start that worked: the banner has nothing left to show.
        self.settings.service.start = ServiceStart::Idle;
        // A sentence from a previous core does not describe this connection.
        self.outcome = None;
        self.push_log(format!("Служба работает ({endpoint})"));
    }

    /// The service writes first-run config before opening this connection; read it now so
    /// Settings does not keep showing the defaults loaded before «Запустить».
    fn reload_first_run_config(&mut self) {
        if !self.config.config_was_missing {
            return;
        }
        let Some(paths) = self.config.paths.clone() else {
            return;
        };
        let fresh = LocalConfig::load(&paths);
        if fresh.config_was_missing {
            return;
        }
        self.report_created_config();
        self.config = fresh;
    }

    /// The link is gone; retain geometry and settings, but not a claimed input target.
    /// Restart completion depends on process/listener exit, not this event.
    pub fn on_disconnected(&mut self, why: String) {
        let lang = self.look.language;
        self.push_log(format!("Связь со службой потеряна: {}", short_error(&why)));
        self.connection = Connection::Disconnected { why };
        if let Some(status) = &mut self.status {
            status.focus.clear();
            status.active_peer.clear();
            status.active_layout.clear();
        }
        self.inflight.clear();
        // History belongs to the lost core session, not to the last known status.
        self.clipboard_history.clear();
        self.silent = false;
        // The banner described what the *core we just lost* did («Готово: …»). Leaving it up
        // under a header that now says «Служба не запущена» is the window contradicting
        // itself, so it goes with the connection (batch-8 audit).
        self.outcome = None;
        // The save in flight died with the link, and a draft nobody can send is not kept.
        self.editor.save = Save::default();
        self.settings.clipboard_pending = None;
        self.settings.discovery_pending = None;
        self.restore_placement();
        // A pairing window belongs to the connection that opened it; the core closes it on
        // its own timeout, and pretending it is still counting down would be a lie.
        self.pairing.window = None;
        self.pairing.prompt = None;
        self.pairing.window_deadline = None;
        self.pairing.dialing = false;
        self.pairing.paired = None;
        self.incoming_pair_request = None;
        self.outgoing_pair_request = None;
        if self.settings.service.stopping.take().is_some() {
            self.settings.service.line = Some(Outcome::done(text(lang, Key::TheServiceIsStopped)));
        }
    }

    /// Begins stopping a connected core through its graceful GUI request.
    pub fn begin_stop(&mut self, now: Instant) -> bool {
        if !self.connection.is_connected()
            || self.settings.service.stopping.is_some()
            || self.settings.service.restart.is_some()
        {
            return false;
        }
        self.settings.service.stopping = Some(now);
        self.settings.service.line = None;
        self.push_log("Служба останавливается");
        true
    }

    /// Appearance controls wait for the service's authoritative status.
    pub fn can_set_appearance(&self) -> bool {
        self.connection.is_connected()
            && self.status.is_some()
            && !self.inflight.iter().any(|request| {
                matches!(request.request, Request::SetAppearance | Request::GetStatus)
            })
    }

    /// Records a request that was just handed to the client.
    pub fn begin_request(&mut self, request: Request, now: Instant) -> u64 {
        self.next_request_id += 1;
        let id = self.next_request_id;
        self.inflight.push(Inflight {
            id,
            request,
            deadline: now
                + match request {
                    // Pairing can wait 120 seconds for a human on the other computer.
                    Request::PairWith | Request::PairingAnswer => Duration::from_secs(122),
                    // An addressed send waits for the core's 30-second apply verdict.
                    Request::SendClipboardHistory => Duration::from_secs(32),
                    _ => REQUEST_TIMEOUT,
                },
            clipboard_target: None,
        });
        if self.inflight.len() > INFLIGHT_LIMIT {
            self.inflight.remove(0);
        }
        id
    }

    /// Online remote members are the core's trusted membership roster, not discovery results.
    pub fn clipboard_targets(&self) -> impl Iterator<Item = &ipc::MemberStatus> {
        self.status.iter().flat_map(|status| {
            status.members.iter().filter(move |member| {
                !status.local_node_id.is_empty()
                    && member.online
                    && member.node_id != status.local_node_id
            })
        })
    }

    /// Remember the destination independently of subsequent status pushes.
    pub fn begin_clipboard_send(&mut self, peer_node_id: &str, now: Instant) -> u64 {
        let target = self
            .clipboard_targets()
            .find(|member| member.node_id == peer_node_id)
            .map(|member| {
                if member.display_name.is_empty() {
                    short_id(&member.node_id)
                } else {
                    member.display_name.clone()
                }
            })
            .unwrap_or_else(|| short_id(peer_node_id));
        let id = self.begin_request(Request::SendClipboardHistory, now);
        if let Some(inflight) = self.inflight.last_mut() {
            inflight.clipboard_target = Some(target);
        }
        id
    }

    /// The client could not put a request on the wire (no core is listening).
    pub fn on_not_sent(&mut self, id: u64) {
        if id == COORDINATOR_REQUEST_ID {
            return;
        }
        let lang = self.look.language;
        let request = self.take_inflight(id);
        if matches!(request, Some(Request::PairWith | Request::PairingAnswer)) {
            self.finish_pairing(false);
        }
        let name = request.map_or(text(lang, Key::Request), |request| request.label(lang));
        self.push_log(format!("Служба не запущена: {name} не отправлено"));
        if request == Some(Request::Shutdown) {
            self.settings.service.stopping = None;
            self.settings.service.restart = None;
            self.settings.service.line = Some(Outcome::failed(
                "служба перестала отвечать до получения просьбы остановиться",
            ));
        }
    }

    /// Uses only this window's pid when a graceful shutdown request has had no effect.
    pub fn take_timed_out_shutdown_pid(&mut self, now: Instant) -> Option<u32> {
        let expired = self
            .settings
            .service
            .stopping
            .is_some_and(|since| now.duration_since(since) >= SERVICE_STOP_TIMEOUT);
        if !expired {
            return None;
        }
        self.settings.service.stopping = None;
        if let Some(index) = self
            .inflight
            .iter()
            .position(|inflight| inflight.request == Request::Shutdown)
        {
            self.inflight.remove(index);
        }
        self.silent = self
            .inflight
            .iter()
            .any(|inflight| now >= inflight.deadline);
        let Some(pid) = self.core.as_ref().and_then(|core| core.pid) else {
            self.settings.service.line = Some(Outcome::failed(
                "служба не ответила на просьбу остановиться; это окно не запускало её и не может остановить процесс",
            ));
            self.push_log("Служба не ответила; чужой процесс оставлен без изменений");
            return None;
        };
        self.settings.service.line = Some(Outcome::failed(
            "служба не ответила на просьбу остановиться; окно завершает свой процесс принудительно",
        ));
        self.push_log(format!(
            "Служба не ответила; окно останавливает свой процесс (pid {pid})"
        ));
        Some(pid)
    }

    /// Records the outcome of the pid-only fallback, used only for this window's process.
    pub fn on_shutdown_fallback(&mut self, stopped: Result<(), String>) {
        if let Err(why) = stopped {
            self.settings.service.line = Some(Outcome::failed(format!(
                "не удалось принудительно остановить службу ({why})"
            )));
            self.push_log(format!("Не удалось принудительно остановить службу: {why}"));
        }
    }

    /// How long until something the window is waiting on runs out, so the app can schedule
    /// exactly one frame for it instead of polling: an unanswered request, a stop that must
    /// give up. `None` when nothing is pending.
    pub fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        let mut at: Option<Instant> = None;
        let mut consider = |when: Instant| {
            if when > now {
                at = Some(at.map_or(when, |best| best.min(when)));
            }
        };
        for inflight in &self.inflight {
            consider(inflight.deadline);
        }
        if let Some(since) = self.settings.service.stopping {
            consider(since + SERVICE_STOP_TIMEOUT);
        }
        if let Some(Restart::Stopping { .. }) = &self.settings.service.restart {
            consider(now + Duration::from_millis(100));
        }
        if let ServiceStart::Starting { since } = self.settings.service.start {
            consider(since + SERVICE_START_TIMEOUT);
        }
        if let (Some(since), false) = (self.outcome_since, self.outcome_hold) {
            consider(since + TOAST_FOR);
        }
        if let Some(flash) = &self.flash {
            consider(flash.until);
        }
        at.map(|when| when - now)
    }

    /// Starts the toast's timer when a toast is first seen and ends it when it has run out;
    /// a toast under the pointer keeps its time.
    fn tick_toast(&mut self, now: Instant) {
        if self.outcome != self.outcome_seen {
            self.outcome_seen.clone_from(&self.outcome);
            self.outcome_since = self.outcome.as_ref().map(|_| now);
        }
        if let (Some(since), false) = (self.outcome_since, self.outcome_hold) {
            if now.saturating_duration_since(since) >= TOAST_FOR {
                self.outcome = None;
                self.outcome_seen = None;
                self.outcome_since = None;
            }
        }
    }

    /// Ages the deadlines; called once per frame.
    pub fn tick(&mut self, now: Instant) {
        self.silent = self
            .inflight
            .iter()
            .any(|inflight| now >= inflight.deadline);
        self.tick_toast(now);
        if self.flash.as_ref().is_some_and(|flash| now >= flash.until) {
            self.flash = None;
        }
        // A save whose request is no longer anywhere is over: nothing will ever answer it.
        if let Some(sent) = &self.editor.save.sent {
            if !self
                .inflight
                .iter()
                .any(|inflight| inflight.id == sent.request_id)
            {
                self.editor.save = Save::default();
                self.restore_placement();
            }
        }
        // A start that is still unanswered past its timeout is a start that failed: the
        // banner stops waiting, says why, and offers «Ещё раз».
        if let ServiceStart::Starting { since } = self.settings.service.start {
            if now.saturating_duration_since(since) >= SERVICE_START_TIMEOUT {
                let reason = fill_named(
                    self.look.language,
                    Key::TheServiceDidNotAnswerInSeconds,
                    &[("seconds", &SERVICE_START_TIMEOUT.as_secs())],
                );
                self.push_log(reason.clone());
                self.settings.service.start = ServiceStart::Failed { reason };
            }
        }
    }

    /// «Запустить» on the «служба не запущена» banner was pressed: the wait begins.
    pub fn begin_service_start(&mut self, now: Instant) {
        self.settings.service.start = ServiceStart::Starting { since: now };
    }

    /// What the «служба не запущена» banner says.
    pub fn service_start(&self) -> &ServiceStart {
        &self.settings.service.start
    }

    /// `true` while the window waits for a core it asked for: the banner shows the wait.
    pub fn waiting_for_core(&self) -> bool {
        self.service_start().is_starting()
            || matches!(self.settings.service.restart, Some(Restart::Starting))
    }

    /// The core process was started, or could not be.
    pub fn on_core_started(&mut self, started: Result<StartedCore, String>) {
        let lang = self.look.language;
        match started {
            Ok(core) => {
                self.push_log(format!(
                    "Служба запущена: {} (pid {})",
                    core.program.display(),
                    core.pid
                        .map_or_else(|| "не сообщён".to_owned(), |pid| pid.to_string())
                ));
                self.core = Some(core);
                self.report_created_config();
                if self.settings.service.restart.take().is_some() {
                    self.settings.service.line = Some(Outcome::done("Служба перезапущена"));
                    // The sentence «Применить» was waiting for: the address is in the file
                    // *and* in use, because the fresh core is the one holding it.
                    if self
                        .settings
                        .lan
                        .applied
                        .as_ref()
                        .is_some_and(|applied| applied.ok)
                    {
                        self.settings.lan.applied =
                            Some(Outcome::done("Адрес сохранён, служба перезапущена"));
                    }
                }
            }
            Err(why) => {
                self.push_log(fill(
                    lang,
                    Key::CouldNotStartTheService,
                    &[&short_error(&why)],
                ));
                self.settings.service.restart = None;
                let reason = fill(lang, Key::CouldNotStartTheService2, &[&short_error(&why)]);
                if self.connection.is_connected() {
                    // A live window has no banner to carry the reason; the toast does.
                    self.set_outcome(Outcome::failed(reason));
                } else {
                    self.settings.service.start = ServiceStart::Failed { reason };
                }
            }
        }
    }

    /// «Настройки созданы автоматически»: once per session, once the window's own core is up.
    ///
    /// `LocalConfig::load` read no file on this machine, and the core writes one on its first
    /// run — so the fact is worth one sentence now that the file exists, on the screen where
    /// the user can see it. A restart of the same window must not say it again.
    fn report_created_config(&mut self) {
        if !self.config.config_was_missing || self.settings.first_run_reported {
            return;
        }
        self.settings.first_run_reported = true;
        self.settings.first_run = true;
        self.push_log("Настройки созданы автоматически (config.toml)");
    }

    // ------------------------------------------------------------ incoming frames

    /// Folds one `GuiFrame` — a response or an event — into the state.
    pub fn apply(&mut self, frame: GuiFrame) {
        let lang = self.look.language;
        let Some(msg) = frame.msg else {
            self.push_log("Служба прислала пустой кадр");
            return;
        };
        // Any frame is proof that the core is there, whatever it says.
        if !self.connection.is_connected() {
            self.connection = Connection::Connected;
        }
        self.silent = false;
        // The read half of the contract answers with the state itself (`Status`,
        // `TopologyDoc`) and still carries the `request_id` it answers, so
        // those requests are retired here; the write half answers with an `OpResult`, which
        // is the one message that has to name its request to be understood.
        if !matches!(&msg, Msg::OpResult(_)) {
            self.take_inflight(frame.request_id);
        }
        match msg {
            Msg::Status(status) => self.apply_status(status),
            Msg::TopologyDoc(doc) => self.apply_topology(doc),
            Msg::PairingWindow(window) => self.apply_pairing_window(window),
            Msg::PairingResult(result) => {
                let paired = result.outcome == ipc::PairingOutcome::Paired as i32;
                self.finish_pairing(paired);
                if paired && !result.peer_name.is_empty() {
                    self.pairing.paired = Some(result.peer_name);
                }
            }
            Msg::IncomingPairingRequest(prompt) => {
                self.incoming_pair_request = (prompt.seconds_remaining > 0).then_some(prompt);
                if self.incoming_pair_request.is_some() {
                    self.modal = None;
                    self.appearance_open = false;
                    self.look.dock_open = true;
                }
            }
            Msg::OutgoingPairingRequest(request) => {
                self.outgoing_pair_request = Some(request);
            }
            Msg::PairingPrompt(prompt) => {
                self.push_log(fill(
                    lang,
                    Key::PairingOffersACodeToCompare,
                    &[&if prompt.peer_name.is_empty() {
                        text(lang, Key::WithoutAName)
                    } else {
                        prompt.peer_name.as_str()
                    }],
                ));
                self.pairing.details = false;
                self.pairing.prompt = Some(prompt);
            }
            Msg::ClipboardHistory(history) => {
                self.clipboard_history = history.items;
                self.clipboard_history
                    .sort_by_key(|entry| std::cmp::Reverse(entry.id));
            }
            // Copy requests are applied by the core; this GUI never needs full text.
            Msg::ClipboardHistoryItem(_) => {}
            Msg::OpResult(result) => self.apply_op_result(frame.request_id, result),
            Msg::HotkeysResult(result) => {
                self.hotkeys_error = (!result.detail.is_empty()).then_some(result.detail);
                if result.success {
                    if let Some(status) = self.status.as_mut() {
                        status.hotkeys = result.bindings;
                    }
                }
            }
            // The pushed half of the contract never arrives here; a GUI that gets one of its
            // own requests back is talking to something that is not a core.
            other => self.push_log(fill(
                lang,
                Key::TheServiceSentAnUnexpectedFrame,
                &[&message_name(&other)],
            )),
        }
    }

    /// Visible unpaired LAN announcements in status order; callers may sort for placement.
    pub fn discovered_peers(&self) -> impl Iterator<Item = &ipc::DiscoveredPeer> {
        self.unpaired_discovered_peers()
            .filter(|peer| !self.look.hidden_discovered.contains(&peer.node_id))
    }

    /// Currently announcing hidden peers, excluding paired nodes and disabled discovery.
    pub fn hidden_discovered_peers(&self) -> impl Iterator<Item = &ipc::DiscoveredPeer> {
        self.unpaired_discovered_peers()
            .filter(|peer| self.look.hidden_discovered.contains(&peer.node_id))
    }

    fn unpaired_discovered_peers(&self) -> impl Iterator<Item = &ipc::DiscoveredPeer> {
        self.status
            .iter()
            .filter(|status| {
                self.connection.is_connected()
                    && status.discovery_enabled
                    && self.discovery_enabled()
            })
            .flat_map(|status| &status.discovered_peers)
            .filter(|peer| {
                !self
                    .editor
                    .nodes
                    .iter()
                    .any(|member| member.node_id == peer.node_id)
                    && !self.status.as_ref().is_some_and(|status| {
                        status
                            .members
                            .iter()
                            .any(|member| member.node_id == peer.node_id)
                    })
            })
    }

    /// Status owns the setting once connected; config supplies the initial value.
    pub fn discovery_enabled(&self) -> bool {
        self.settings.discovery_pending.unwrap_or_else(|| {
            self.status
                .as_ref()
                .map_or(self.config.discovery_enabled, |status| {
                    status.discovery_enabled
                })
        })
    }

    /// `Status` is a response and an event, and both are the same fold.
    fn apply_status(&mut self, status: ipc::Status) {
        for member in &status.members {
            self.app_versions.insert(
                member.node_id.clone(),
                protocol::sanitize_app_version(&member.app_version),
            );
        }
        let lang = self.look.language;
        if status.unpair_notice_id == 0 {
            // A restarted core has no notice yet and begins a fresh sequence.
            self.unpair_notice_seen = 0;
            self.unpair_notice = None;
        } else if status.unpair_notice_id > self.unpair_notice_seen {
            self.unpair_notice_seen = status.unpair_notice_id;
            if !status.unpair_notice.is_empty() {
                self.unpair_notice = Some(status.unpair_notice.clone());
            }
        }
        if self
            .status
            .as_ref()
            .is_none_or(|old| old.layout_revision != status.layout_revision)
        {
            self.push_log(fill(
                lang,
                Key::LayoutRevisionFrom,
                &[
                    &status.layout_revision,
                    &author_label(lang, &status.layout_author),
                ],
            ));
        }
        self.editor.editable = status.layout_editable;
        // The pushed status settles the clipboard switch even while its own `SetClipboard`
        // is still on the wire: core commits the toggle and then answers, so the window must
        // not wait for the `OpResult` (or ask again) to leave the pending state.
        if self.settings.clipboard_pending.is_some()
            && (self.settings.clipboard_pending == Some(status.clipboard_enabled)
                || !self
                    .inflight
                    .iter()
                    .any(|inflight| inflight.request == Request::SetClipboard))
        {
            self.settings.clipboard_pending = None;
        }
        self.config.discovery_enabled = status.discovery_enabled;
        self.config.accept_pairing_requests = status.accept_pairing_requests;
        if self.settings.discovery_pending.is_some()
            && (self.settings.discovery_pending == Some(status.discovery_enabled)
                || !self
                    .inflight
                    .iter()
                    .any(|request| request.request == Request::SetDiscovery))
        {
            self.settings.discovery_pending = None;
        }
        // A status announces a revision, not its links. Advance the CAS base only when its
        // TopologyDoc arrives; otherwise a queued drop could race ahead on stale geometry.
        self.status = Some(status);
    }

    /// A whole document: the editor's base, as long as it is not older than the one held.
    ///
    /// The user's draft survives a push of the revision it was made on (the core pushes the
    /// document on membership changes too); it is replaced only when the revision moves.
    fn apply_topology(&mut self, doc: ipc::TopologyDoc) {
        for member in &doc.members {
            self.app_versions.insert(
                member.node_id.clone(),
                protocol::sanitize_app_version(&member.app_version),
            );
        }
        let reloaded = std::mem::take(&mut self.editor.save.reloading);
        self.fold_topology(doc);
        if reloaded {
            // The conflict's retry: the latest draft, on top of the document just read.
            self.apply_draft();
        }
        self.continue_save();
    }

    fn fold_topology(&mut self, doc: ipc::TopologyDoc) {
        let lang = self.look.language;
        if doc.revision < self.editor.revision {
            self.push_log(fill(
                lang,
                Key::LayoutRevisionIsOlderThanTheCurrentThePreviousOneWasKept,
                &[&doc.revision, &self.editor.revision],
            ));
            return;
        }
        self.editor.forgotten_nodes = doc.forgotten_node_ids.clone();
        let same_members = self.editor.nodes.len() == doc.members.len()
            && self
                .editor
                .nodes
                .iter()
                .zip(&doc.members)
                .all(|(node, member)| node.node_id == member.node_id);
        let same_geometry =
            same_members
                && self
                    .editor
                    .nodes
                    .iter()
                    .zip(&doc.members)
                    .all(|(node, member)| {
                        node.displays.len() == member.displays.len()
                            && node.displays.iter().zip(&member.displays).all(
                                |(display, report)| {
                                    display.id == report.id
                                        && display.x == finite(report.logical_x, 0.0)
                                        && display.y == finite(report.logical_y, 0.0)
                                        && display.w == finite(report.logical_w, 0.0)
                                        && display.h == finite(report.logical_h, 0.0)
                                        && display.primary == report.primary
                                },
                            )
                    });
        let retain_placement = same_geometry
            && (self.editor.links == doc.links
                || self.editor.save.sent.is_some()
                || self.editor.save.awaiting.is_some());
        if doc.revision == self.editor.revision && self.editor.dirty() && same_geometry {
            // Same revision, same computers: only who is online and how they are called can
            // have changed, and a draft the user is holding must not be wiped for that.
            for (node, member) in self.editor.nodes.iter_mut().zip(&doc.members) {
                node.online = member.online;
                node.display_name.clone_from(&member.display_name);
                node.system_name = protocol::sanitize_system_name(&member.system_name);
                for (display, report) in node.displays.iter_mut().zip(&member.displays) {
                    display.native_known = report.native_known;
                    display.native_w = f64::from(report.native_width);
                    display.native_h = f64::from(report.native_height);
                    display.scale = finite(report.scale, 1.0).max(f64::EPSILON);
                    display.rotation = report.rotation_degrees;
                }
            }
            return;
        }
        self.editor.revision = doc.revision;
        self.editor.author = doc.author_node_id.clone();
        self.editor.links = doc
            .links
            .iter()
            .filter(|link| link.from.is_some() && link.to.is_some())
            .cloned()
            .collect();
        let nodes: Vec<EditorNode> = doc
            .members
            .iter()
            .map(|member| EditorNode {
                node_id: member.node_id.clone(),
                display_name: member.display_name.clone(),
                system_name: protocol::sanitize_system_name(&member.system_name),
                online: member.online,
                displays: member
                    .displays
                    .iter()
                    .map(EditorDisplay::from_view)
                    .collect(),
            })
            .collect();
        self.editor.nodes = nodes;
        // An unchanged manifest or our own CAS acknowledgment must not move a
        // freely placed computer. A changed display report rebuilds real geometry.
        if !retain_placement {
            if !same_geometry {
                self.editor.save.placement = None;
            }
            self.rebuild_layout();
        }
        self.push_log(fill(
            lang,
            Key::LayoutReceivedRevisionFromMembers,
            &[
                &doc.revision,
                &author_label(lang, &doc.author_node_id),
                &doc.members.len(),
            ],
        ));
    }

    /// The pairing window opened, changed what is left of it, or closed.
    fn apply_pairing_window(&mut self, window: ipc::PairingWindow) {
        let lang = self.look.language;
        if window.open {
            self.push_log(fill(
                lang,
                Key::ThePairingWindowIsOpenS,
                &[&window.address, &window.seconds_remaining],
            ));
            self.pairing.window_deadline =
                Some(Instant::now() + Duration::from_secs(u64::from(window.seconds_remaining)));
            self.pairing.window = Some(window);
        } else {
            self.push_log("Окно сопряжения закрыто");
            self.pairing.window = None;
            self.pairing.window_deadline = None;
            self.pairing.prompt = None;
        }
    }

    /// A ceremony ended: the code and the dialling state are over, and a success stays on
    /// screen until «Готово».
    fn finish_pairing(&mut self, paired: bool) {
        let lang = self.look.language;
        let name = self
            .pairing
            .prompt
            .take()
            .map(|prompt| prompt.peer_name)
            .filter(|name| !name.is_empty());
        self.pairing.dialing = false;
        self.pairing.paired = None;
        if paired {
            self.pairing.paired =
                Some(name.unwrap_or_else(|| text(lang, Key::ComputerTitle).to_owned()));
            self.pairing.window = None;
            self.pairing.window_deadline = None;
        }
    }

    /// The verdict of one request.
    fn apply_op_result(&mut self, request_id: u64, result: ipc::OpResult) {
        if request_id == COORDINATOR_REQUEST_ID {
            return;
        }
        let lang = self.look.language;
        let clipboard_target = self
            .inflight
            .iter()
            .find(|inflight| inflight.id == request_id)
            .and_then(|inflight| inflight.clipboard_target.clone());
        let Some(request) = self.take_inflight(request_id) else {
            self.push_log(fill(
                lang,
                Key::AnAnswerToAnUnknownRequest,
                &[&code_label(lang, &result.error_code)],
            ));
            return;
        };
        if request == Request::SendClipboardHistory {
            let target = clipboard_target.as_deref().unwrap_or("");
            let line = fill_named(
                lang,
                if result.ok {
                    Key::ClipboardSentTo
                } else {
                    Key::ClipboardSendFailed
                },
                &[("target", &target), ("detail", &result.error_code)],
            );
            self.push_log(line.clone());
            self.set_outcome(if result.ok {
                Outcome::done(line)
            } else {
                Outcome::failed(line)
            });
            return;
        }
        if request == Request::UpdateTopology {
            // A verdict that arrives late is still the verdict of the one save in flight: it is
            // matched by id, not by time, so it can never leave the draft stuck.
            let retrying = self.on_layout_verdict(request_id, &result);
            if result.ok {
                self.push_log(format!("{}: готово", request.label(lang)));
                return;
            }
            let failure = failure_text(lang, request, &result.error_code);
            self.push_log(format!("{}: {}", request.label(lang), failure));
            if !retrying {
                self.set_outcome(Outcome::failed(failure));
            }
            return;
        }
        if result.ok {
            let line = self.success_text(request);
            // A toggle shows its own state; a sentence under it says nothing new.
            if !matches!(
                request,
                Request::SetClipboard
                    | Request::SetClipboardPolicy
                    | Request::SetAppearance
                    | Request::SetDiscovery
                    | Request::PairingRequest
                    | Request::SetPairingRequests
                    | Request::PairingAnswer
            ) {
                self.set_outcome(Outcome::done(line.clone()));
            }
            if request == Request::Shutdown {
                self.settings.service.line =
                    Some(Outcome::done(text(lang, Key::TheServiceIsStopping)));
            }
            match request {
                Request::ExportDiagnostics => {
                    self.settings.export = Some(Outcome::done(line.clone()));
                }
                Request::PairWith => self.pairing.dialing = false,
                Request::PairingAnswer => {
                    self.pairing.prompt = None;
                    self.pairing.dialing = false;
                }
                Request::ClearClipboardHistory => {
                    self.clipboard_history.clear();
                    let id = self.begin_request(Request::GetClipboardHistory, Instant::now());
                    self.outgoing
                        .push((id, Msg::GetClipboardHistory(ipc::GetClipboardHistory {})));
                }
                Request::SetClipboardPolicy
                | Request::SetAppearance
                | Request::SetDiscovery
                | Request::SetPairingRequests
                | Request::SetGameGuard => {
                    let id = self.begin_request(Request::GetStatus, Instant::now());
                    self.outgoing.push((id, Msg::GetStatus(ipc::GetStatus {})));
                }
                _ => {}
            }
            self.push_log(format!("{}: готово ({line})", request.label(lang)));
            return;
        }
        let code = result.error_code.as_str();
        let failure = failure_text(lang, request, code);
        self.push_log(format!("{}: {}", request.label(lang), failure));
        if request == Request::SetClipboard {
            self.settings.clipboard_pending = None;
        }
        if request == Request::SetDiscovery {
            self.settings.discovery_pending = None;
        }
        if request == Request::Shutdown {
            self.settings.service.stopping = None;
            self.settings.service.line = Some(Outcome::failed(failure.clone()));
        }
        if request == Request::ExportDiagnostics {
            self.settings.export = Some(Outcome::failed(failure.clone()));
        }
        if matches!(request, Request::PairWith | Request::PairingAnswer) {
            self.finish_pairing(false);
        }
        self.set_outcome(Outcome::failed(failure));
    }

    /// Shows `outcome` as the toast; its timer starts when it is first drawn.
    pub fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }

    /// A quiet note on the map for `how_long`.
    pub fn flash(&mut self, text: impl Into<String>, how_long: Duration) {
        self.flash = Some(Flash {
            text: text.into(),
            until: Instant::now() + how_long,
        });
    }

    /// Takes the requests the view model made itself, to be sent as they are.
    pub fn take_outgoing(&mut self) -> Vec<(u64, Msg)> {
        std::mem::take(&mut self.outgoing)
    }

    /// The sentence a successful request is reported with.
    ///
    /// The pairing answer names the peer; the live link status arrives separately in `Status`.
    fn success_text(&self, request: Request) -> String {
        let lang = self.look.language;
        if request == Request::PairingAnswer {
            let name = self
                .pairing
                .prompt
                .as_ref()
                .map(|prompt| prompt.peer_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| text(lang, Key::ComputerTitle).to_owned());
            return fill_named(lang, Key::NameIsPaired, &[("name", &name)]);
        }
        success_text(lang, request)
    }

    /// Removes and returns the request `id` names.
    fn take_inflight(&mut self, id: u64) -> Option<Request> {
        let index = self
            .inflight
            .iter()
            .position(|inflight| inflight.id == id)?;
        Some(self.inflight.remove(index).request)
    }

    // ------------------------------------------------------------ derived line

    /// The header's status line, in the words of someone who did not install anything: the
    /// thing that must be running is «служба», not «ядро».
    pub fn connection_label(&self) -> &'static str {
        let lang = self.look.language;
        if self.silent {
            text(lang, Key::TheServiceIsNotAnswering)
        } else if self.connection.is_connected() {
            text(lang, Key::TheServiceIsRunning)
        } else {
            text(lang, Key::TheServiceIsNotRunning)
        }
    }

    /// Why the link is down, when it is.
    pub fn disconnected_reason(&self) -> Option<&str> {
        match &self.connection {
            Connection::Disconnected { why } if !self.silent => Some(why),
            _ => None,
        }
    }

    /// «Куда идёт ввод» (an NFR of `docs/03-architecture.md`) — the headline the status
    /// screen shows first and largest, because it is the one thing the window exists for.
    pub fn focus_line(&self) -> String {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return text(lang, Key::NoDataFromTheService).to_owned();
        };
        match status.focus.as_str() {
            "" => text(lang, Key::NoDataFromTheService).to_owned(),
            "LOCAL" => text(lang, Key::InputHere).to_owned(),
            "PREPARING" => text(lang, Key::PreparingTheSwitch).to_owned(),
            "REMOTE" => {
                let peer = if status.active_peer.is_empty() {
                    text(lang, Key::TheOtherComputer).to_owned()
                } else {
                    status.active_peer.clone()
                };
                fill_named(lang, Key::InputOnPeer, &[("peer", &peer)])
            }
            "BLOCKED" => text(lang, Key::InputStoppedBecauseOfAnError).to_owned(),
            other => fill_named(
                lang,
                Key::InputTheServiceReportedAnUnknownStateOther,
                &[("other", &other)],
            ),
        }
    }

    /// The small line under [`ViewModel::focus_line`]: the one fact that does not fit the
    /// headline, which is the keyboard layout in use on the other computer.
    pub fn focus_detail(&self) -> Option<String> {
        let lang = self.look.language;
        let status = self.status.as_ref()?;
        (status.focus == "REMOTE" && !status.active_layout.is_empty())
            .then(|| fill(lang, Key::Layout2, &[&status.active_layout]))
    }

    /// Whether the link to the other computer is up.
    pub fn link_line(&self) -> &'static str {
        let lang = self.look.language;
        match &self.status {
            Some(status) if status.connected => text(lang, Key::Connected),
            Some(_) => text(lang, Key::Disconnected),
            None => text(lang, Key::NoDataFromTheService),
        }
    }

    /// The links the core marked inactive because one of their displays is not in the live
    /// manifests, as plain lines for the GUI. Empty when every link is usable.
    ///
    /// The link is not lost: it becomes active again on its own once the monitor is reported.
    pub fn inactive_link_lines(&self) -> Vec<String> {
        let lang = self.look.language;
        self.editor
            .links
            .iter()
            .filter(|link| !link.active)
            .map(|link| {
                if link.inactive_reason.is_empty() {
                    fill(lang, Key::LinkIsInactiveTheDisplayWasNotFound, &[&link.id])
                } else {
                    fill(
                        lang,
                        Key::LinkIsInactiveDisplayWasNotFound,
                        &[&link.id, &link.inactive_reason],
                    )
                }
            })
            .collect()
    }

    /// One line per link anchored at a whole desktop instead of at a named display.
    ///
    /// «Весь рабочий стол (авто)» says what the link is, and the screens listed after it are
    /// what the core resolves it to against the live manifests right now — the monitors on the
    /// outer edge of each side. Empty when every link names its displays.
    pub fn auto_link_lines(&self) -> Vec<String> {
        let lang = self.look.language;
        self.editor
            .links
            .iter()
            .filter_map(|link| {
                let from = link.from.as_ref()?;
                let to = link.to.as_ref()?;
                let anchored =
                    |endpoint: &ipc::EndpointView| endpoint.display_id == config::AUTO_DISPLAY;
                if !anchored(from) && !anchored(to) {
                    return None;
                }
                let screens = if link.auto_displays.is_empty() {
                    text(lang, Key::TheDisplaysHaveNotArrivedYet).to_owned()
                } else {
                    link.auto_displays.join(", ")
                };
                Some(fill_named(
                    lang,
                    Key::LinkLinkTheWholeDesktopAutoScreens,
                    &[("link", &link.id), ("screens", &screens)],
                ))
            })
            .collect()
    }

    /// One line per **other** computer, or the sentence that says there are none.
    ///
    /// This computer is not a peer of itself: listing it here read as a third machine and the
    /// user asked why. Who controls the keyboard is said in plain words, «владелец» being
    /// the core's word and not the reader's.
    pub fn member_lines(&self) -> Vec<String> {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return vec![text(lang, Key::NoDataFromTheService).to_owned()];
        };
        let this = self.self_node().map(str::to_owned);
        let others: Vec<&protocol::ipc::MemberStatus> = status
            .members
            .iter()
            .filter(|member| !self.is_this_computer(member, this.as_deref()))
            .collect();
        if others.is_empty() {
            return vec![text(lang, Key::NobodyYetOpenThePairingWindow).to_owned()];
        }
        others
            .into_iter()
            .map(|member| {
                let name = if member.display_name.is_empty() {
                    fill(lang, Key::ComputerNamed, &[&short_id(&member.node_id)])
                } else {
                    member.display_name.clone()
                };
                let state = if member.online {
                    text(lang, Key::Online)
                } else {
                    text(lang, Key::Offline)
                };
                let keyboard = if member.is_owner {
                    text(lang, Key::OwnsTheKeyboard)
                } else {
                    text(lang, Key::DoesNotOwnTheKeyboard)
                };
                format!("{name} — {state}, {keyboard}")
            })
            .collect()
    }

    /// Whether one member of `Status` is this computer, as far as the window can tell.
    ///
    /// [`ViewModel::self_node`] answers from the *document* (a node id it can place), which is
    /// what naming a topology endpoint needs. A list of the **other** computers needs less:
    /// `Status.is_owner` says the owner is this node, so the owner's id is this computer's id
    /// even before any document arrived. Without either, the question stays open and the core's
    /// own list is shown unedited rather than guessed at.
    fn is_this_computer(&self, member: &protocol::ipc::MemberStatus, this: Option<&str>) -> bool {
        if let Some(this) = this {
            return member.node_id == this;
        }
        let Some(status) = &self.status else {
            return false;
        };
        status.is_owner
            && !status.owner_node_id.is_empty()
            && member.node_id == status.owner_node_id
    }

    /// `true` when the core named `code` among the capabilities the active backend cannot
    /// provide. A window greys out the control that would use it rather than offering a switch
    /// that cannot do anything.
    pub fn capability_missing(&self, code: &str) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status
                .capabilities_unavailable
                .iter()
                .any(|named| named == code)
        })
    }

    /// `true` when this system's desktop has portals to forget: the Wayland backend, and only
    /// it, so the window shows the «Порталы» row on Wayland and on nothing else.
    pub fn uses_portals(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.backend == "platform-wayland")
    }

    /// The backend in the words the owner reads.
    pub fn backend_label(&self) -> String {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return text(lang, Key::Unknown).to_owned();
        };
        match status.backend.as_str() {
            "platform-win" => "Windows".to_owned(),
            "platform-x11" => "X11 (Xorg)".to_owned(),
            "platform-wayland" => "Wayland".to_owned(),
            "" => text(lang, Key::Unknown).to_owned(),
            other => other.to_owned(),
        }
    }

    /// The short id of this computer, when the `Status` names it.
    ///
    /// `Status.local_node_id` is this node's own id and is what the core sends now. A core
    /// from before the field existed leaves it empty and `owner_node_id` only means this
    /// computer while this computer is the owner; anything else shows the display name
    /// alone, because the build spec is explicit («не выдумывайте id»).
    pub fn short_id_of_self(&self) -> Option<String> {
        let status = self.status.as_ref()?;
        if !status.local_node_id.is_empty() {
            return Some(short_id(&status.local_node_id));
        }
        // While this node is the owner, `owner_node_id` is this node's id and the claim is
        // exact; otherwise the field names the *other* computer and showing it as this one's
        // id would be the invented id the build spec forbids.
        if status.is_owner && !status.owner_node_id.is_empty() {
            Some(short_id(&status.owner_node_id))
        } else {
            None
        }
    }

    /// Who owns input, as the two lines the block shows.
    pub fn owner_lines(&self) -> (String, String) {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return (
                text(lang, Key::NoDataFromTheService2).to_owned(),
                String::new(),
            );
        };
        // The serial is the one number in this block and the build spec asks for it small;
        // it is shown whoever owns input, because it is what a support question is about.
        let serial = if status.owner_serial == 0 {
            String::new()
        } else {
            fill(lang, Key::TheOwnerSSession, &[&status.owner_serial])
        };
        if status.is_owner {
            return (text(lang, Key::ThisComputerTitle).to_owned(), serial);
        }
        let name = status
            .members
            .iter()
            .find(|member| member.is_owner)
            .map(|member| {
                if member.display_name.is_empty() {
                    fill(lang, Key::ComputerNamed, &[&short_id(&member.node_id)])
                } else {
                    member.display_name.clone()
                }
            })
            .or_else(|| {
                if status.owner_node_id.is_empty() {
                    None
                } else {
                    Some(fill(
                        lang,
                        Key::ComputerNamed,
                        &[&short_id(&status.owner_node_id)],
                    ))
                }
            })
            .unwrap_or_else(|| text(lang, Key::NoOwner).to_owned());
        (name, serial)
    }

    /// Why the core reports degraded input service, or `None` while it is healthy.
    pub fn agent_degraded_reason(&self) -> Option<&str> {
        let lang = self.look.language;
        let status = self.status.as_ref()?;
        if !status.agent_degraded_reason.is_empty() {
            Some(status.agent_degraded_reason.as_str())
        } else if status.agent_restart_circuit_open {
            Some(text(lang, Key::TheInputServiceIsStopped))
        } else {
            None
        }
    }

    /// The «Ограничения и деградации» block (KVM-056): never invented, only `Status`.
    pub fn limits(&self) -> Vec<String> {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return vec![text(lang, Key::NoDataFromTheService).to_owned()];
        };
        let mut lines = Vec::new();
        for code in &status.capabilities_unavailable {
            lines.push(match code.as_str() {
                "EMERGENCY_HOTKEY" => text(
                    lang,
                    Key::NoEmergencyKeyTheKeyboardCanOnlyBeReleasedWithTheMouseHere,
                )
                .to_owned(),
                "RECEIVER_TAKEOVER" => text(
                    lang,
                    Key::PhysicalInputHereDoesNotEndTheSessionByItselfOnlyTheSourceCanSwitch,
                )
                .to_owned(),
                "TEXT_CLIPBOARD" => text(lang, Key::TheSharedClipboardIsUnavailable).to_owned(),
                "ABSOLUTE_POINTER" => text(
                    lang,
                    Key::ThisComputerDoesNotAcceptAnAbsolutePointerPosition,
                )
                .to_owned(),
                "CAPTURE" => text(lang, Key::ThisComputerCannotCaptureInput).to_owned(),
                "INJECT" => text(lang, Key::ThisComputerCannotInjectEvents).to_owned(),
                other => fill_named(lang, Key::LimitationOther, &[("other", &other)]),
            });
        }
        match status.portal_state.as_str() {
            "restoring" => lines.push(
                text(lang, Key::ThePortalGrantIsBeingRestoredFromTheSavedTokenNoDialogIsExpected)
                    .to_owned(),
            ),
            "permission_required" => lines.push(
                text(lang, Key::APortalPermissionDialogIsExpectedConfirmItOrThisNodeCannotTakeInput)
                    .to_owned(),
            ),
            "denied" => lines.push(
                text(lang, Key::AccessWasDeniedInThePermissionDialogPressForgetThePortalGrantsInTheSettingsAndTryAgain)
                    .to_owned(),
            ),
            "unavailable" => lines.push(text(lang, Key::DesktopPortalsAreUnavailableOnThisSystem).to_owned()),
            _ => {}
        }
        if status.backend == "platform-wayland" {
            // Only what Wayland as a platform means. The rest of what it costs — the
            // missing takeover, the emergency hotkey — arrives as the capability codes
            // above, if the core could not provide them: on Wayland where they *are*
            // provided (a desktop that supports them), this line must not claim otherwise.
            lines.push(
                text(
                    lang,
                    Key::WaylandInputCanOnlyBeCapturedThroughDesktopPortals,
                )
                .to_owned(),
            );
        }
        if !status.last_error_code.is_empty() {
            lines.push(fill(
                lang,
                Key::LastMessage,
                &[&code_label(lang, &status.last_error_code)],
            ));
        }
        lines
    }

    /// The latency line, or `None` while nobody measures it.
    ///
    /// `rtt_p95_us` is the core's «known» marker: with no answered pings it is zero, and a zero
    /// line would claim a round trip of «0 мс» instead of saying nothing. Once it is known, the
    /// jitter is a measurement too (a steady link reports 0), so both travel together.
    pub fn delay_line(&self) -> Option<String> {
        let lang = self.look.language;
        let status = self.status.as_ref()?;
        if status.rtt_p95_us == 0 {
            return None;
        }
        Some(fill(
            lang,
            Key::DelayJitterMs,
            &[
                &(status.rtt_p95_us / 1_000),
                &(status.rtt_jitter_us / 1_000),
            ],
        ))
    }

    /// «Открыть файл настроек»: `config.toml` in the system's editor; a failure is a toast.
    pub fn open_config_file(&mut self) {
        let lang = self.look.language;
        if let Err(why) = open_in_editor(&self.config.config_file) {
            self.set_outcome(Outcome::failed(fill(
                lang,
                Key::CouldNotOpenTheSettingsFile,
                &[&short_error(&why)],
            )));
        }
    }

    /// Appends a line to the window's log.
    pub fn push_log(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    // ------------------------------------------------------------ the editor

    /// The member this computer is, when the window can tell.
    ///
    /// `Status.local_node_id` names this node outright, which is what makes the answer work
    /// on a follower in a three-node mesh. Older cores leave the field empty, so the two
    /// fallbacks stay: while this node is the owner, `owner_node_id` is itself; otherwise,
    /// with two members and exactly one of them marked as the owner, the other one is this
    /// computer. Anything else — three members, no owner known, no local id — is left
    /// unknown rather than guessing.
    pub fn self_node(&self) -> Option<&str> {
        let status = self.status.as_ref()?;
        let known = |node_id: &str| self.editor.nodes.iter().any(|node| node.node_id == node_id);
        if !status.local_node_id.is_empty() && known(&status.local_node_id) {
            return Some(status.local_node_id.as_str());
        }
        if status.is_owner && known(&status.owner_node_id) {
            return Some(status.owner_node_id.as_str());
        }
        if self.editor.nodes.len() == 2 {
            let owner = self
                .editor
                .nodes
                .iter()
                .find(|node| {
                    status
                        .members
                        .iter()
                        .any(|member| member.node_id == node.node_id && member.is_owner)
                })
                .map(|node| node.node_id.as_str());
            if let Some(owner) = owner {
                return self
                    .editor
                    .nodes
                    .iter()
                    .map(|node| node.node_id.as_str())
                    .find(|node_id| *node_id != owner);
            }
        }
        None
    }

    /// The edited document as `[topology]` TOML — what `UpdateTopology` carries.
    ///
    /// The schema is the one `crates/config` defines for `[topology]`
    /// (`docs/config.example.toml`), so the core parses it as
    /// `config::TopologyConfig` and the file this line would be a section of parses as
    /// `config::Config`: `revision > 0` needs an author, the floor never trails the applied
    /// revision, a link id is unique, an endpoint is `"self"` or `"peer:<64 hex digits>"`,
    /// and every segment is finite with `0 <= start < end`.
    pub fn layout_toml(&self) -> String {
        let topology = self.topology_with(self.editor_links());
        // Only a non-finite number can get here, and the fields are sanitised on the way in;
        // an empty document is refused by the core with a sentence the user can read, which
        // beats panicking in a status window.
        toml::to_string(&topology).unwrap_or_default()
    }

    /// The editor's model in the configuration schema.
    fn topology_with(&self, links: Vec<config::LinkConfig>) -> config::TopologyConfig {
        let mut topology = config::TopologyConfig {
            revision: self.editor.revision,
            author: author_reference(&self.editor.author, self.editor.revision),
            max_seen_revision: self.editor.revision,
            max_seen_author: author_reference(&self.editor.author, self.editor.revision),
            displays: Vec::new(),
            links: Vec::new(),
            forgotten_nodes: self.editor.forgotten_nodes.clone(),
        };
        for node in &self.editor.nodes {
            let reference = node_reference(&node.node_id);
            for display in &node.displays {
                topology.displays.push(config::DisplayConfig {
                    node: reference.clone(),
                    id: display.id.clone(),
                    logical: [display.x, display.y, display.w, display.h],
                    primary: display.primary,
                });
            }
        }
        topology.links = links;
        topology
    }

    // ------------------------------------------------------------ local actions

    /// Applies everything in an [`Action`] that never reaches the core.
    ///
    /// The wire requests are the app's business (it hands them to [`GuiClient`]); this is the
    /// half that changes what the window shows: which screen is open, what the user is
    /// typing, where they dragged a rectangle.
    pub fn local(&mut self, action: &Action) {
        match action {
            Action::ToggleModal(Modal::Appearance) => self.local(&Action::ToggleAppearance),
            Action::ToggleModal(modal) => {
                self.modal = if self.modal == Some(*modal) {
                    None
                } else {
                    Some(*modal)
                };
                if self.modal == Some(Modal::Clip) {
                    let id = self.begin_request(Request::GetClipboardHistory, Instant::now());
                    self.outgoing
                        .push((id, Msg::GetClipboardHistory(ipc::GetClipboardHistory {})));
                }
                self.appearance_open = false;
                self.settings.revoke = None;
            }
            Action::ToggleAppearance => {
                self.appearance_open = !self.appearance_open;
                self.modal = None;
            }
            Action::ToggleDock => self.look.dock_open = !self.look.dock_open,
            // «Открыть окно сопряжения»: the window of the rail closes and the dock, which
            // shows the address and the code, is brought out if it was folded.
            Action::StartPairing { .. } => {
                self.modal = None;
                self.appearance_open = false;
                self.look.dock_open = true;
            }
            Action::SetClipboard(enabled) => self.settings.clipboard_pending = Some(*enabled),
            Action::SetDiscovery(enabled) => self.settings.discovery_pending = Some(*enabled),
            Action::ToggleGameGuardApps => {
                self.settings.game_guard_apps_open = !self.settings.game_guard_apps_open
            }
            Action::HoldToast(hold) => {
                if *hold != self.outcome_hold {
                    let now = Instant::now();
                    if *hold {
                        self.outcome_hold_since = Some(now);
                    } else if let (Some(start), Some(since)) =
                        (self.outcome_hold_since.take(), self.outcome_since.as_mut())
                    {
                        *since += now.saturating_duration_since(start);
                    }
                    self.outcome_hold = *hold;
                }
            }
            Action::DropNode(node_id) => self.drop_node(node_id),
            Action::MovePaddle {
                link_id,
                from,
                start,
                end,
            } => {
                self.move_paddle(link_id, *from, *start, *end);
            }
            Action::DropPaddle => self.drop_paddle(),
            Action::CancelDrag => self.restore_placement(),
            Action::CloseModal => {
                self.modal = None;
                self.appearance_open = false;
                self.settings.revoke = None;
            }
            Action::SetLook(look) => self.look = look.clone(),
            Action::HideDiscovered(node_id) => {
                self.look.hidden_discovered.hide(node_id);
            }
            Action::ShowDiscovered(node_id) => {
                self.look.hidden_discovered.remove(node_id);
            }
            Action::DonePairing => self.pairing.paired = None,
            Action::RequestPairing(_) => {
                self.modal = None;
                self.appearance_open = false;
                self.look.dock_open = true;
                self.pairing.paired = None;
            }
            Action::PairWith(address) => {
                self.modal = None;
                self.appearance_open = false;
                self.look.dock_open = true;
                self.pairing.address = address.clone();
                self.pairing.dialing = true;
                self.pairing.paired = None;
            }
            Action::DismissOutcome => {
                self.outcome = None;
                self.outcome_hold = false;
            }
            Action::DismissUnpairNotice => self.unpair_notice = None,
            Action::SetAddress(address) => self.pairing.address = address.clone(),
            Action::ToggleDetails(open) => self.pairing.details = *open,
            Action::MoveNode { node_id, x, y } => {
                self.move_node(node_id, *x, *y);
            }
            Action::NudgeNode { node_id, dx, dy } => {
                // A key press is a whole edit: it is dropped where it lands.
                self.nudge_node(node_id, *dx, *dy);
                self.drop_node(node_id);
            }
            Action::SelectRevoke(node_id) => {
                self.settings.revoke = node_id.clone();
            }
            Action::SelectLanAddress(address) => self.settings.lan.choice = Some(address.clone()),
            Action::ApplyLanAddress => self.apply_lan_address(),
            // «Перезапустить службу» is not a fold: it decides against the pid the window
            // holds and asks for a process to be stopped, so `Bridge::dispatch` drives it.
            Action::RestartCore => {}
            Action::DismissFirstRun => self.settings.first_run = false,
            _ => {}
        }
    }

    /// «Применить» on the LAN address: writes `network.listen_address`, says what happened.
    ///
    /// The write goes through the config crate's own writer (`first_run::set_listen_address`),
    /// so the file this window otherwise only reads is changed by the code that owns its
    /// rules. A host with no resolved directories or no chosen address is refused in words
    /// rather than guessed at, and a failed write is a sentence with the reason in it.
    fn apply_lan_address(&mut self) {
        let lang = self.look.language;
        let current = self.config.listen_address.clone();
        let chosen = self.settings.lan.selected(&current).unwrap_or_default();
        let Some(address) = chosen.parse::<std::net::Ipv4Addr>().ok() else {
            self.settings.lan.applied = Some(Outcome::failed(text(
                lang,
                Key::NoAddressIsSelectedPickOneInTheListAndPressApplyAgain,
            )));
            return;
        };
        let Some(paths) = self.config.paths.clone() else {
            self.settings.lan.applied = Some(Outcome::failed(text(
                lang,
                Key::TheSettingsDirectoriesAreUnknownRestartTheWindowWithoutSpecialFlags,
            )));
            return;
        };
        match config::first_run::set_listen_address(&paths, address) {
            Ok(()) => {
                self.push_log(format!("Адрес этого компьютера: {address}"));
                self.config.listen_address = address.to_string();
                self.settings.lan.choice = None;
                self.settings.lan.applied = Some(Outcome::done(text(lang, Key::TheAddressIsSaved)));
            }
            Err(why) => {
                self.settings.lan.applied = Some(Outcome::failed(fill(
                    lang,
                    Key::TheAddressWasNotSaved,
                    &[&short_error(&why)],
                )));
            }
        }
    }

    /// Restart any connected core gracefully; an owned PID is only a timeout fallback.
    pub fn begin_restart(&mut self, now: Instant) -> RestartStep {
        if self.settings.service.stopping.is_some() {
            return RestartStep::Refused;
        }
        if self.settings.service.restart.is_some() {
            return RestartStep::Refused;
        }
        self.settings.service.line = None;
        self.settings.service.restart = Some(Restart::Stopping { since: now });
        self.push_log("Служба перезапускается");
        if self.connection.is_connected() {
            RestartStep::Stop
        } else {
            RestartStep::Start
        }
    }

    /// What the stop of the core process answered, when it answered at all.
    ///
    /// A stop that failed ends the restart: the window says why, and leaves the service
    /// alone. A sent stop leaves [`Restart::Stopping`] waiting for process/listener exit.
    pub fn on_core_stopped(&mut self, stopped: Result<(), String>) {
        let lang = self.look.language;
        if let Err(why) = stopped {
            self.settings.service.restart = None;
            self.push_log(format!("Не удалось остановить службу: {why}"));
            self.settings.service.line = Some(Outcome::failed(fill_named(
                lang,
                Key::CouldNotStopTheServiceWhy2,
                &[("why", &short_error(&why))],
            )));
        }
    }
}

// ---------------------------------------------------------------- actions

/// Everything the window can ask for.
///
/// The first group travels to the core (as the `ipc.proto` request it names); the last group
/// is applied to the view model by the app. [`Action::wire`] is the boundary between them.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A GUI-only updater operation; intercepted by the app, never sent to the agent.
    Update(crate::updater::Action),
    /// Core orchestrates all peers before replacing this computer.
    UpdateAll(String),
    SetRemoteUpdate(bool),
    RegisterUpdateCoordinator(u32),
    /// Ask the core for its state and the document again.
    Refresh,
    /// Become the owner of input.
    Claim,
    /// Give ownership up and stay a follower.
    Abandon,
    /// Open the pairing window for `window_s` seconds.
    StartPairing {
        /// Seconds the core keeps the window open (the protocol caps it at 120).
        window_s: u32,
    },
    /// Dial the other computer's pairing address.
    PairWith(String),
    RequestPairing(ipc::RequestPairing),
    AnswerPairingRequest {
        requester_node_id: Vec<u8>,
        accept: bool,
    },
    CancelPairingRequest,
    SetPairingRequests(bool),
    /// Hide an unpaired LAN identity locally, without trusting or changing the peer.
    HideDiscovered(String),
    /// Restore a locally hidden identity, including one no longer announcing.
    ShowDiscovered(String),
    /// Answer the six-digit confirmation.
    Answer {
        /// The attempt the prompt named.
        attempt_id: Vec<u8>,
        /// `true` for «Да, совпадают».
        approve: bool,
    },
    /// Turn the shared clipboard on or off.
    SetClipboard(bool),
    /// Persist routing mode and the current service-owned settling delay.
    SetClipboardPolicy {
        policy: String,
        settle_ms: u32,
    },
    /// Persist LAN discovery through the core.
    SetDiscovery(bool),
    SetGameGuard(ipc::SetGameGuard),
    ToggleGameGuardApps,
    /// Refresh metadata without fetching clipboard contents.
    RefreshClipboardHistory,
    /// Copy a stored item to this computer's clipboard through the core.
    CopyClipboardHistory(u64),
    /// Send a stored item to one remote member; never retrieves its text.
    SendClipboardHistory {
        id: u64,
        peer_node_id: String,
    },
    /// Forget all stored clipboard contents.
    ClearClipboardHistory,
    /// Change the service-owned edge effects and source cursor visibility.
    SetAppearance {
        /// Draw the linked-edge glyph strip.
        edge_effects: bool,
        /// Hide the source cursor during remote input.
        hide_remote_cursor: bool,
    },
    /// Change a core-owned shortcut binding.
    SetHotkey {
        /// `ipc::HotkeyAction`.
        action: i32,
        /// Canonical chord text, or empty to disable an optional action.
        chord: String,
    },
    /// Apply the edited layout document.
    UpdateTopology {
        /// The document, in the `[topology]` schema.
        layout_toml: Vec<u8>,
        /// The revision the document was edited against.
        expected_revision: u64,
        /// The author of that revision; empty for the first document.
        expected_author: Vec<u8>,
    },
    /// Forget the portal permissions of this node.
    ResetPortalGrants,
    /// Ask the core to retry its degraded input agent after the user presses «Повторить».
    RetryAgent,
    /// Revoke a paired computer.
    RevokePeer(Vec<u8>),
    /// Ask the core to write its diagnostics.
    ExportDiagnostics(String),
    /// Start `lws-core run` for this user, or try again after a start that failed.
    StartCore,
    /// Open the window of a rail section, or close it when it is the one already open.
    ToggleModal(Modal),
    /// Close the window over the map.
    CloseModal,
    /// Change the look (theme, field, clouds); the app writes it to the GUI's settings file.
    SetLook(crate::gui_settings::Look),
    /// Leave the «Пара создана» message.
    DonePairing,
    /// Forget the last finished operation.
    DismissOutcome,
    /// Acknowledge the remote unpair notice without reopening it on another status.
    DismissUnpairNotice,
    /// The pointer is on (`true`) or left (`false`) the toast: its timer waits.
    HoldToast(bool),
    /// Open or close the appearance panel.
    ToggleAppearance,
    /// Slide the pairing dock in or out.
    ToggleDock,
    /// A computer drag ended: save the layout it made.
    DropNode(String),
    /// Adjust one endpoint without moving either computer; absolute stage coordinates.
    MovePaddle {
        link_id: String,
        from: bool,
        start: f64,
        end: f64,
    },
    /// Save the latest paddle draft through the same single-flight CAS as node drops.
    DropPaddle,
    /// A drag was cut short (focus lost, the pointer left the window): nothing is saved and the
    /// computer goes back.
    CancelDrag,
    /// Open `config.toml` in the system's default editor.
    OpenConfigFile,
    /// The line of «Подключиться по адресу».
    SetAddress(String),
    /// Open or close the fingerprints under the code (ADR-010).
    ToggleDetails(bool),
    /// Freely drag a computer's top-left corner to `(x,y)` stage units.
    MoveNode {
        /// The member.
        node_id: String,
        /// Wanted left, in stage units.
        x: f64,
        /// Wanted top, in stage units.
        y: f64,
    },
    /// Move a computer of the map by a keyboard step.
    NudgeNode {
        /// The member.
        node_id: String,
        /// Horizontal step, stage units.
        dx: f64,
        /// Vertical step, stage units.
        dy: f64,
    },
    /// Choose the member to revoke.
    SelectRevoke(Option<String>),
    /// Pick the LAN address the dropdown shows.
    SelectLanAddress(String),
    /// Write `network.listen_address` and restart the service behind it.
    ApplyLanAddress,
    /// Ask the core to stop itself, regardless of which window started it.
    StopCore,
    /// Stop the core this window started and start a fresh one.
    RestartCore,
    /// Close «Настройки созданы автоматически».
    DismissFirstRun,
    /// Register or remove this executable at user login.
    SetAutostart(bool),
}

/// Unknown versions never advertise an update; build metadata does not affect precedence.
pub fn version_is_older(current: &str, highest: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(highest),
    ) {
        (Ok(current), Ok(highest)) => current.cmp_precedence(&highest).is_lt(),
        _ => false,
    }
}

impl Action {
    /// Discovery supplies hints; core verifies the selected TLS certificate identity.
    pub fn connect_discovered(peer: &ipc::DiscoveredPeer) -> Self {
        Self::RequestPairing(ipc::RequestPairing {
            address: peer.address.clone(),
            node_id: peer.node_id.clone(),
            display_name: peer.display_name.clone(),
            supports_requests: peer.supports_pairing_requests,
        })
    }

    /// The request this action puts on the wire, or `None` for a local one.
    pub fn wire(&self) -> Option<(Request, Msg)> {
        let wire = match self {
            Action::HideDiscovered(node_id) => (
                Request::PairingRequest,
                Msg::MutePairingRequester(ipc::MutePairingRequester {
                    node_id: node_id.clone(),
                }),
            ),
            Action::Refresh => (Request::GetStatus, Msg::GetStatus(ipc::GetStatus {})),
            Action::UpdateAll(target_version) => (
                Request::UpdateAll,
                Msg::UpdateAll(ipc::UpdateAll {
                    target_version: target_version.clone(),
                }),
            ),
            Action::SetRemoteUpdate(enabled) => (
                Request::SetRemoteUpdate,
                Msg::SetRemoteUpdate(ipc::SetRemoteUpdate { enabled: *enabled }),
            ),
            Action::RegisterUpdateCoordinator(pid) => (
                Request::RegisterUpdateCoordinator,
                Msg::UpdateProgress(ipc::UpdateProgress {
                    target_version: String::new(),
                    status: "coordinator".into(),
                    detail: pid.to_string(),
                }),
            ),
            Action::Claim => (Request::Claim, Msg::ClaimOwnership(ipc::ClaimOwnership {})),
            Action::Abandon => (
                Request::Abandon,
                Msg::AbandonOwnership(ipc::AbandonOwnership {}),
            ),
            Action::StartPairing { window_s } => (
                Request::StartPairing,
                Msg::StartPairing(ipc::StartPairing {
                    window_s: *window_s,
                }),
            ),
            Action::RequestPairing(request) => (
                Request::PairingRequest,
                Msg::RequestPairing(request.clone()),
            ),
            Action::AnswerPairingRequest {
                requester_node_id,
                accept,
            } => (
                Request::PairingRequest,
                Msg::AnswerPairingRequest(ipc::AnswerPairingRequest {
                    requester_node_id: requester_node_id.clone(),
                    accept: *accept,
                }),
            ),
            Action::CancelPairingRequest => (
                Request::PairingRequest,
                Msg::CancelPairingRequest(ipc::CancelPairingRequest {}),
            ),
            Action::SetPairingRequests(enabled) => (
                Request::SetPairingRequests,
                Msg::SetPairingRequests(ipc::SetPairingRequests { enabled: *enabled }),
            ),
            Action::PairWith(address) => (
                Request::PairWith,
                Msg::PairWith(ipc::PairWith {
                    address: address.clone(),
                }),
            ),
            Action::Answer {
                attempt_id,
                approve,
            } => (
                Request::PairingAnswer,
                Msg::PairingAnswer(ipc::PairingAnswer {
                    attempt_id: attempt_id.clone(),
                    approve: *approve,
                }),
            ),
            Action::SetClipboard(enabled) => (
                Request::SetClipboard,
                Msg::SetClipboard(ipc::SetClipboard { enabled: *enabled }),
            ),
            Action::SetClipboardPolicy { policy, settle_ms } => (
                Request::SetClipboardPolicy,
                Msg::SetClipboardPolicy(ipc::SetClipboardPolicy {
                    policy: policy.clone(),
                    settle_ms: *settle_ms,
                }),
            ),
            Action::SetDiscovery(enabled) => (
                Request::SetDiscovery,
                Msg::SetDiscovery(ipc::SetDiscovery { enabled: *enabled }),
            ),
            Action::SetGameGuard(set) => (Request::SetGameGuard, Msg::SetGameGuard(set.clone())),
            Action::RefreshClipboardHistory => (
                Request::GetClipboardHistory,
                Msg::GetClipboardHistory(ipc::GetClipboardHistory {}),
            ),
            Action::CopyClipboardHistory(id) => (
                Request::CopyClipboardHistory,
                Msg::GetClipboardHistoryItem(ipc::GetClipboardHistoryItem {
                    id: *id,
                    copy_to_local: true,
                }),
            ),
            Action::SendClipboardHistory { id, peer_node_id } => (
                Request::SendClipboardHistory,
                Msg::SendClipboardHistoryItem(ipc::SendClipboardHistoryItem {
                    id: *id,
                    peer_node_id: node_id_bytes(peer_node_id),
                }),
            ),
            Action::ClearClipboardHistory => (
                Request::ClearClipboardHistory,
                Msg::ClearClipboardHistory(ipc::ClearClipboardHistory {}),
            ),
            Action::SetAppearance {
                edge_effects,
                hide_remote_cursor,
            } => (
                Request::SetAppearance,
                Msg::SetAppearance(ipc::SetAppearance {
                    edge_effects: *edge_effects,
                    hide_remote_cursor: *hide_remote_cursor,
                }),
            ),
            Action::SetHotkey { action, chord } => (
                Request::SetHotkey,
                Msg::SetHotkey(ipc::SetHotkey {
                    action: *action,
                    chord: chord.clone(),
                }),
            ),
            Action::UpdateTopology {
                layout_toml,
                expected_revision,
                expected_author,
            } => (
                Request::UpdateTopology,
                Msg::UpdateTopology(ipc::UpdateTopology {
                    layout_toml: layout_toml.clone(),
                    expected_revision: *expected_revision,
                    expected_author: expected_author.clone(),
                }),
            ),
            Action::ResetPortalGrants => (
                Request::ResetPortalGrants,
                Msg::ResetPortalGrants(ipc::ResetPortalGrants {}),
            ),
            Action::RetryAgent => (Request::RetryAgent, Msg::RetryAgent(ipc::RetryAgent {})),
            Action::RevokePeer(node_id) => (
                Request::RevokePeer,
                Msg::RevokePeer(ipc::RevokePeer {
                    peer_node_id: node_id.clone(),
                }),
            ),
            Action::ExportDiagnostics(target) => (
                Request::ExportDiagnostics,
                Msg::ExportDiagnostics(ipc::ExportDiagnostics {
                    target_path: target.clone(),
                }),
            ),
            Action::StopCore => (Request::Shutdown, Msg::Shutdown(ipc::Shutdown {})),
            _ => return None,
        };
        Some(wire)
    }

    /// A second request this action also needs, when it needs one.
    ///
    /// [`Action::Refresh`] asks for both the status and the document, so it has two.
    pub fn extra_wire(&self) -> Option<(Request, Msg)> {
        match self {
            Action::Refresh => Some((Request::GetTopology, Msg::GetTopology(ipc::GetTopology {}))),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- the bridge

/// The window's two halves, wired: the state that draws and the client that talks.
///
/// This is the whole of the wire/local split of the module docs, in one place and out of the
/// `eframe::App`, so that the same pair of methods runs in the window and in a test with no
/// display and no GPU: `apps/lws-gui/tests/cores.rs` drives two real cores through a `Bridge`
/// exactly as [`crate::app::GuiApp`] does. Neither half is touched from anywhere else — the
/// screens only ever produce [`Action`]s, which is what keeps drawing code decision-free.
pub struct Bridge {
    view: ViewModel,
    client: GuiClient,
    restart_wait: Option<(crate::core_exit::Wait, bool)>,
    peer_pid: Option<u32>,
    core_unit: Option<String>,
    restart_unit: Option<String>,
}

impl Bridge {
    /// Wires a view model to the client that serves it.
    pub fn new(view: ViewModel, client: GuiClient) -> Self {
        Bridge {
            view,
            client,
            restart_wait: None,
            peer_pid: None,
            core_unit: None,
            restart_unit: None,
        }
    }

    /// The state that draws.
    pub fn view(&self) -> &ViewModel {
        &self.view
    }

    /// The state that draws, to be changed: [`ViewModel::tick`] ages the request deadlines
    /// once per frame, and that is a decision too.
    pub fn view_mut(&mut self) -> &mut ViewModel {
        &mut self.view
    }

    /// The client that talks, for the two buttons that need no request of their own.
    pub fn client(&self) -> &GuiClient {
        &self.client
    }

    /// Updater shutdown never invokes the service UI's force-stop fallback.
    pub(crate) fn shutdown_for_update(&mut self, now: Instant) {
        let id = self.view.begin_request(Request::Shutdown, now);
        self.client.send(id, Msg::Shutdown(ipc::Shutdown {}));
    }

    /// Sends one action where it belongs.
    ///
    /// Fire-and-forget, as `docs/03-architecture.md` («GUI ↔ core») requires: the request is
    /// recorded against `now` and handed to the client thread, and the answer arrives later as
    /// a frame [`Bridge::pump`] folds.
    pub fn dispatch(&mut self, action: Action, now: Instant) {
        self.dispatch_one(action, now);
        self.send_outgoing();
    }

    /// Puts on the wire what the view model asked for itself: the automatic save, a re-read.
    fn send_outgoing(&mut self) {
        for (id, message) in self.view.take_outgoing() {
            self.client.send(id, message);
        }
    }

    fn dispatch_one(&mut self, action: Action, now: Instant) {
        // The local half first: none of the local actions care whether the wire half below
        // succeeds.
        self.view.local(&action);
        if matches!(&action, Action::StopCore) && !self.view.begin_stop(now) {
            return;
        }
        if let Some((request, message)) = action.wire() {
            let id = if request == Request::RegisterUpdateCoordinator {
                COORDINATOR_REQUEST_ID
            } else if let Action::SendClipboardHistory { peer_node_id, .. } = &action {
                self.view.begin_clipboard_send(peer_node_id, now)
            } else {
                self.view.begin_request(request, now)
            };
            self.client.send(id, message);
            // «Обновить» on the status screen asks for the document as well as the status.
            if let Some((request, message)) = action.extra_wire() {
                let id = self.view.begin_request(request, now);
                self.client.send(id, message);
            }
            return;
        }
        match action {
            Action::StartCore => {
                self.view.begin_service_start(now);
                self.client.start_core();
            }
            Action::RestartCore => self.restart_core(now),
            Action::SetAutostart(enabled) => match crate::autostart::set_enabled(enabled) {
                Ok(()) => {
                    self.view.settings.autostart = enabled;
                    self.view.settings.autostart_error = None;
                }
                Err(error) => self.view.settings.autostart_error = Some(error.to_string()),
            },
            Action::OpenConfigFile => self.view.open_config_file(),
            // «Применить» has already written the file above (`local`), and a saved address
            // is not in use until the core that reads it is the fresh one.
            Action::ApplyLanAddress
                if self
                    .view
                    .settings
                    .lan
                    .applied
                    .as_ref()
                    .is_some_and(|applied| applied.ok) =>
            {
                self.restart_core(now);
            }
            _ => {}
        }
    }

    /// Graceful IPC works for externally started cores too. Never launch on link loss alone.
    fn restart_core(&mut self, now: Instant) {
        let step = self.view.begin_restart(now);
        if step == RestartStep::Refused {
            return;
        }
        let result = self
            .view
            .config
            .paths
            .as_ref()
            .ok_or_else(|| {
                "Cannot wait for core shutdown: settings directories are unknown".to_owned()
            })
            .and_then(|paths| {
                crate::core_exit::Wait::new(
                    paths,
                    self.peer_pid
                        .or_else(|| self.view.core.as_ref().and_then(|core| core.pid)),
                    now,
                )
            });
        match result {
            Ok(wait) => {
                self.restart_wait = Some((wait, false));
                self.restart_unit = self.core_unit.clone();
                if step == RestartStep::Stop {
                    self.shutdown_for_update(now);
                }
            }
            Err(error) => self.view.on_core_stopped(Err(error)),
        }
    }

    fn poll_restart(&mut self, now: Instant) {
        if self.view.settings.service.restart.is_none() {
            self.restart_wait = None;
        }
        let Some((wait, fallback_used)) = &self.restart_wait else {
            return;
        };
        match wait.poll(now) {
            crate::core_exit::Step::Waiting => {}
            // Process exit can precede delivery of the client's EOF event. Do not enqueue
            // StartCore while its command loop is still serving that old connection.
            crate::core_exit::Step::Exited if self.view.connection.is_connected() => {
                if wait.timed_out(now) {
                    self.restart_wait = None;
                    self.view.on_core_stopped(Err(
                        "Core IPC client did not disconnect within 10 seconds".into(),
                    ));
                }
            }
            crate::core_exit::Step::Exited => {
                self.restart_wait = None;
                self.view.settings.service.restart = Some(Restart::Starting);
                self.client.start_core_in_unit(self.restart_unit.take());
            }
            crate::core_exit::Step::TimedOut => {
                // Only terminate a PID this GUI actually launched, and only once.
                let pid = self.view.core.as_ref().and_then(|core| core.pid);
                if !fallback_used {
                    if let Some(pid) = pid {
                        match stop_core(pid).and_then(|()| {
                            let paths = self
                                .view
                                .config
                                .paths
                                .as_ref()
                                .ok_or("Unknown settings directories")?;
                            crate::core_exit::Wait::new(paths, Some(pid), now)
                        }) {
                            Ok(wait) => {
                                self.restart_wait = Some((wait, true));
                                self.view.settings.service.restart =
                                    Some(Restart::Stopping { since: now });
                                return;
                            }
                            Err(error) => self.view.on_core_stopped(Err(error)),
                        }
                    }
                }
                self.restart_wait = None;
                self.view.on_core_stopped(Err(
                    "Core did not exit or release its IPC endpoints within 10 seconds".into(),
                ));
            }
        }
    }

    /// Folds everything the client thread has said since the last frame.
    ///
    /// Returns whether anything arrived: a panel measures itself from the content it drew, so
    /// a line that appears for the first time (the outcome banner, a pairing code) is drawn on
    /// the *next* pass — which is why the window asks for one immediately instead of waiting
    /// out its own repaint interval.
    pub fn pump(&mut self) -> bool {
        let mut arrived = false;
        for event in self.client.drain() {
            arrived = true;
            match event {
                GuiEvent::Connected {
                    endpoint,
                    pid,
                    unit,
                } => {
                    // systemd Restart= may already have supplied the replacement.
                    if self.restart_wait.is_some()
                        && self.restart_unit.is_some()
                        && pid.is_some()
                        && pid != self.peer_pid
                    {
                        self.restart_wait = None;
                        self.restart_unit = None;
                        self.view.settings.service.restart = Some(Restart::Starting);
                        self.view.on_core_started(Ok(StartedCore {
                            program: own_program(),
                            pid: None,
                        }));
                    }
                    self.peer_pid = pid;
                    self.core_unit = unit;
                    self.view.on_connected(&endpoint);
                    // §4 of the build spec: a fresh link asks for the status and the document.
                    self.dispatch(Action::Refresh, Instant::now());
                }
                GuiEvent::Disconnected { why } => {
                    self.view.on_disconnected(why);
                }
                GuiEvent::Frame(frame) => {
                    self.view.apply(*frame);
                    self.send_outgoing();
                }
                GuiEvent::NotSent { id } => self.view.on_not_sent(id),
                GuiEvent::CoreStarted { started } => self.view.on_core_started(started),
            }
        }
        if let Some(pid) = self.view.take_timed_out_shutdown_pid(Instant::now()) {
            self.view.on_shutdown_fallback(stop_core(pid));
        }
        self.poll_restart(Instant::now());
        arrived
    }
}

// ---------------------------------------------------------------- the client

/// One message from the client thread to the window.
#[derive(Debug)]
pub enum GuiEvent {
    /// The link is up; the greeting frames follow.
    Connected {
        /// The endpoint, for the log.
        endpoint: String,
        /// Local peer process, where the OS exposes it.
        pid: Option<u32>,
        /// Dedicated systemd user service whose MainPID is the core.
        unit: Option<String>,
    },
    /// The link is down, with the reason.
    Disconnected {
        /// What the transport reported.
        why: String,
    },
    /// One frame from the core.
    ///
    /// Boxed because a `GuiFrame` is by far the largest thing on this channel and every other
    /// event is a few words: the window sends one request at a time, so the extra allocation
    /// per *frame* costs nothing next to the frame's own decoding.
    Frame(Box<GuiFrame>),
    /// A request could not be sent, because no core is listening.
    NotSent {
        /// The `request_id` the window already recorded.
        id: u64,
    },
    /// «Запустить» finished, one way or the other.
    CoreStarted {
        /// The process, or why it could not be started.
        started: Result<StartedCore, String>,
    },
}

/// What the client thread is told to do.
#[derive(Debug)]
enum Command {
    /// Put one request on the wire.
    Send {
        /// The `request_id` the window recorded.
        id: u64,
        /// The frame's message half, boxed for the same reason as [`GuiEvent::Frame`].
        message: Box<Msg>,
    },
    /// Start `lws-core run` detached.
    StartCore(Option<String>),
    /// Drop the current wait and dial again.
    ConnectAgain,
}

/// The async half of the window: one connection task on its own thread.
///
/// It owns the reconnect loop of the build spec: with no core listening it retries every
/// [`RETRY_DELAY`] and reports [`GuiEvent::Disconnected`], so a core started elsewhere makes
/// the window come alive without the user touching anything.
pub struct GuiClient {
    /// Requests on their way to the thread; `None` after [`Drop`] closed the channel.
    commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    /// Frames on their way back.
    events: tokio::sync::mpsc::UnboundedReceiver<GuiEvent>,
    /// The thread running the runtime; kept so the channel is not the only thing alive.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Called from the client thread after every event, so the window repaints on an IPC
    /// event and at no other time.
    wake: std::sync::Arc<std::sync::OnceLock<Wake>>,
}

/// What wakes the window: `egui::Context::request_repaint`, as a plain closure so the client
/// does not know the toolkit.
type Wake = Box<dyn Fn() + Send + Sync>;

/// The sending half of the event channel, with the wake-up that goes with every event.
#[derive(Clone)]
struct Events {
    tx: tokio::sync::mpsc::UnboundedSender<GuiEvent>,
    wake: std::sync::Arc<std::sync::OnceLock<Wake>>,
}

impl Events {
    fn send(&self, event: GuiEvent) -> Result<(), tokio::sync::mpsc::error::SendError<GuiEvent>> {
        let sent = self.tx.send(event);
        if sent.is_ok() {
            if let Some(wake) = self.wake.get() {
                wake();
            }
        }
        sent
    }
}

impl GuiClient {
    /// Starts the client thread for `endpoint`.
    ///
    /// `endpoint` is `None` when the endpoint could not be resolved: the thread then only
    /// answers «не отправлено», which is exactly what the window says on that host.
    pub fn start(endpoint: Option<Endpoint>, launch: Launch) -> Self {
        let lang = Lang::from_system();
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let wake = std::sync::Arc::new(std::sync::OnceLock::new());
        let event_tx = Events {
            tx: event_tx,
            wake: std::sync::Arc::clone(&wake),
        };
        let thread = std::thread::Builder::new()
            .name("lws-gui-ipc".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = event_tx.send(GuiEvent::Disconnected {
                            why: fill_named(
                                lang,
                                Key::CouldNotStartTalkingToTheServiceError,
                                &[("error", &error)],
                            ),
                        });
                        return;
                    }
                };
                runtime.block_on(worker(endpoint, launch, command_rx, event_tx));
            })
            .ok();
        GuiClient {
            commands: Some(commands),
            events,
            thread,
            wake,
        }
    }

    /// Sets what the client calls after every event (once; a second call is ignored).
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.wake.set(Box::new(wake));
    }

    /// Hands one request to the connection thread.
    pub fn send(&self, id: u64, message: Msg) -> bool {
        self.commands.as_ref().is_some_and(|commands| {
            commands
                .send(Command::Send {
                    id,
                    message: Box::new(message),
                })
                .is_ok()
        })
    }

    /// Asks the thread to start the core process.
    pub fn start_core(&self) -> bool {
        self.start_core_in_unit(None)
    }

    fn start_core_in_unit(&self, unit: Option<String>) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.send(Command::StartCore(unit)).is_ok())
    }

    /// Asks the thread to dial again now.
    pub fn connect_again(&self) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.send(Command::ConnectAgain).is_ok())
    }

    /// Takes everything the thread has said since the last frame was drawn.
    pub fn drain(&mut self) -> Vec<GuiEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for GuiClient {
    fn drop(&mut self) {
        // Dropping the sender ends the worker's `select!` on a closed channel; the thread
        // returns and the runtime with it. Joining here keeps the window's shutdown ordered,
        // and the thread never blocks on the network — every await in it is cancellable.
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The client thread: connect, serve, retry.
async fn worker(
    endpoint: Option<Endpoint>,
    launch: Launch,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: Events,
) {
    let Some(endpoint) = endpoint else {
        // Nothing to connect to: the host has no resolvable endpoint at all. The window
        // still has to answer its buttons.
        while let Some(command) = commands.recv().await {
            answer_offline(command, &launch, &events);
        }
        return;
    };
    // A core that stays down fails the same way every 2 s; the window is told once, not on
    // every try, so an idle window with no service is not woken every 2 s.
    let mut last_failure: Option<String> = None;
    loop {
        match ::ipc::transport::connect(&endpoint).await {
            Ok(stream) => {
                last_failure = None;
                #[cfg(unix)]
                let pid = ::ipc::transport::peer_credentials(&stream)
                    .ok()
                    .and_then(|peer| peer.pid);
                #[cfg(not(unix))]
                let pid = None;
                let unit = pid.and_then(crate::core_exit::systemd_unit);
                let _ = events.send(GuiEvent::Connected {
                    endpoint: endpoint.describe(),
                    pid,
                    unit,
                });
                let mut link = ::ipc::GuiLink::new(stream);
                // §4 of the build spec: on connect, ask for the status **and** the document.
                // The app does that when it sees `Connected` (one request counter, one place
                // that records deadlines), so this loop only carries the link.
                if let Some(why) = serve(&mut link, &mut commands, &events).await {
                    let _ = events.send(GuiEvent::Disconnected { why });
                }
            }
            Err(error) => {
                let why = error.to_string();
                if last_failure.as_deref() != Some(why.as_str()) {
                    last_failure = Some(why.clone());
                    let _ = events.send(GuiEvent::Disconnected { why });
                }
            }
        }
        // The retry wait, which also serves the buttons: a request can only be reported as
        // unsent while there is no link, and «Запустить»/«Подключиться снова» end the wait.
        let mut wake = Box::pin(tokio::time::sleep(RETRY_DELAY));
        loop {
            tokio::select! {
                () = &mut wake => break,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::ConnectAgain) => break,
                    Some(Command::StartCore(unit)) => {
                        let started = launch_core_in_unit(&launch, unit.as_deref());
                        let _ = events.send(GuiEvent::CoreStarted { started });
                        // The core needs a moment before its endpoint exists; the retry below
                        // covers the rest, and on Windows `connect` already waits for a pipe.
                        wake.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(500));
                    }
                    Some(Command::Send { id, .. }) => {
                        let _ = events.send(GuiEvent::NotSent { id });
                    }
                },
            }
        }
    }
}

/// Serves one live link until it breaks.
///
/// The `select!` returns a value instead of running handlers inside the branches: `link` is
/// borrowed for the whole `select!`, and calling `link.send` while that borrow is alive would
/// not compile. Both futures are cancel-safe (`Framed` keeps its read buffer across calls),
/// so a branch that loses the race loses nothing.
///
/// The stream is generic because the two platforms have different types on the two ends:
/// core accepts a `ClientStream`, while the window dials a `NamedPipeClient` / `UnixStream`.
async fn serve<S>(
    link: &mut ::ipc::GuiLink<S>,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
    events: &Events,
) -> Option<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let lang = Lang::from_system();
    loop {
        // The frame is boxed as it leaves `recv`, and the error is turned into its text here,
        // so the value the `select!` returns is small: a `GuiFrame` is by far the largest
        // thing on either side of this loop.
        enum Step {
            Frame(Box<GuiFrame>),
            Failed(String),
            Command(Option<Command>),
        }
        let step = tokio::select! {
            frame = link.recv() => match frame {
                Ok(frame) => Step::Frame(Box::new(frame)),
                Err(error) => Step::Failed(error.to_string()),
            },
            command = commands.recv() => Step::Command(command),
        };
        match step {
            Step::Frame(frame) => {
                if events.send(GuiEvent::Frame(frame)).is_err() {
                    return None;
                }
            }
            Step::Failed(error) => return Some(error),
            Step::Command(None) => return None,
            Step::Command(Some(Command::Send { id, message })) => {
                if let Err(error) = link.send(id, *message).await {
                    return Some(error.to_string());
                }
            }
            Step::Command(Some(Command::ConnectAgain)) => return None,
            Step::Command(Some(Command::StartCore(unit))) => {
                // systemd may have restarted the service before this queued start arrived.
                let started = if unit.is_some() {
                    Ok(StartedCore {
                        program: own_program(),
                        pid: None,
                    })
                } else {
                    Err(text(lang, Key::TheServiceIsAlreadyRunningAndAnswering).to_owned())
                };
                let _ = events.send(GuiEvent::CoreStarted { started });
            }
        }
    }
}

/// Answers a command while no link exists.
fn answer_offline(command: Command, launch: &Launch, events: &Events) {
    match command {
        Command::Send { id, .. } => {
            let _ = events.send(GuiEvent::NotSent { id });
        }
        Command::StartCore(unit) => {
            let started = launch_core_in_unit(launch, unit.as_deref());
            let _ = events.send(GuiEvent::CoreStarted { started });
        }
        Command::ConnectAgain => {}
    }
}

// ---------------------------------------------------------------- starting a core

/// The flags a core started from this window has to be given.
///
/// The window and the daemon have to agree on `config/`, `data/`, `state/` and `run/`, so the
/// flags are carried through verbatim instead of being rebuilt from [`Paths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The executable that is started with `<flags> run`: `bifrost`, which is this
    /// process's own executable unless a test names another.
    pub program: PathBuf,
    /// The global flags, exactly as `bifrost` wants them.
    pub flags: Vec<String>,
}

impl Launch {
    /// A launch that names the documented directories, started from this very executable.
    pub fn with_flags(flags: Vec<String>) -> Self {
        Launch {
            program: own_program(),
            flags,
        }
    }
}

/// The global options of the command line, exactly as `lws-core` reads them.
///
/// One parser for every role ([`config::CliPaths`]): the window is started with the
/// arguments, the tray gets the window's own arguments unchanged, and the core is started by
/// the window with [`CliArgs::flags`]. Both `--root DIR` and `--root=DIR` therefore name the
/// same directory everywhere; before the shared parser they did not, and `bifrost
/// --root=/path` left the window on the default directories while the tray used `/path`.
///
/// The window ignores an argument it does not know (a window that refuses to open because of
/// a stray argument helps nobody); the core's own command-line rules stay in the core.
pub use config::CliPaths as CliArgs;

/// The endpoint of the core for `paths`, or the reason there is none.
pub fn resolve_endpoint(paths: &Paths) -> (CoreEndpoint, Option<Endpoint>) {
    match ::ipc::transport::gui_endpoint(paths) {
        Ok(endpoint) => (
            CoreEndpoint::Ready {
                description: endpoint.describe(),
            },
            Some(endpoint),
        ),
        Err(error) => (
            CoreEndpoint::Failed {
                why: error.to_string(),
            },
            None,
        ),
    }
}

/// The program a window-started core runs as: this very executable, `bifrost`, in its
/// command-line role (`bifrost <flags> run`).
fn own_program() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| {
        PathBuf::from(if cfg!(windows) {
            "bifrost.exe"
        } else {
            "bifrost"
        })
    })
}

/// Opens `file` in the system's default editor: `explorer` on Windows, `xdg-open` on Linux.
pub fn open_in_editor(file: &Path) -> Result<(), String> {
    let lang = Lang::from_system();
    if file.as_os_str().is_empty() || !file.exists() {
        return Err(text(lang, Key::TheSettingsFileWasNotFound).to_owned());
    }
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(opener)
        .arg(file)
        .spawn()
        .map_err(|error| format!("{opener}: {error}"))?;
    // Reaped on a thread of its own: the editor outlives the click, the zombie must not.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Stops the core process this window started, by the pid [`StartedCore`] names.
///
/// Windows has no signals, so it is `taskkill /PID <pid> /F` — the documented way to stop a
/// process, and `/F` is what makes it unconditional, because the core has no console to be
/// asked through. On Unix it is `kill -TERM` and deliberately not `KILL`: the core treats
/// SIGTERM as the clean shutdown its own `systemd` unit asks for, closing its listeners and
/// writing what it has, whereas SIGKILL would leave the endpoint bound until the kernel
/// reaped the process and the fresh core could not take the address over.
///
/// The outcome is the window's to report: [`ViewModel::on_core_stopped`] says why in words
/// when the stop could not be sent at all.
pub fn stop_core(pid: u32) -> Result<(), String> {
    let lang = Lang::from_system();
    #[cfg(windows)]
    let (program, mut command) = {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/F"]);
        // The window has no console of its own (`main.rs`), so neither does the helper that
        // serves it: without this flag a black slab would flash behind the button.
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        ("taskkill", command)
    };
    #[cfg(not(windows))]
    let (program, mut command) = {
        let mut command = std::process::Command::new("kill");
        command.arg("-TERM").arg(pid.to_string());
        ("kill", command)
    };
    // `taskkill` announces what it did on its own streams; nothing in this window prints to a
    // console (`main.rs`), and the exit status is the only part of the answer the window can
    // read anyway.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(stop_core_failure(lang, program, pid, status.code())),
        Err(error) => Err(fill_named(
            lang,
            Key::ProgramDidNotStartError,
            &[("program", &program), ("error", &error)],
        )),
    }
}

fn stop_core_failure(lang: Lang, program: &str, pid: u32, code: Option<i32>) -> String {
    let code = code.map_or_else(
        || text(lang, Key::Unknown2).to_owned(),
        |code| code.to_string(),
    );
    fill_named(
        lang,
        Key::ProgramDidNotStopProcessPidCode,
        &[("program", &program), ("pid", &pid), ("code", &code)],
    )
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[test]
    fn stop_failure_names_the_program_pid_and_exit_code_in_both_languages() {
        for (lang, expected) in [
            (Lang::Ru, "taskkill не остановил процесс 4242 (код 5)"),
            (Lang::En, "taskkill did not stop process 4242 (code 5)"),
        ] {
            assert_eq!(stop_core_failure(lang, "taskkill", 4242, Some(5)), expected);
        }
    }

    #[test]
    fn stop_failure_localizes_an_unknown_exit_code() {
        assert_eq!(
            stop_core_failure(Lang::Ru, "kill", 4242, None),
            "kill не остановил процесс 4242 (код неизвестен)"
        );
        assert_eq!(
            stop_core_failure(Lang::En, "kill", 4242, None),
            "kill did not stop process 4242 (code unknown)"
        );
    }
}

/// The addresses this computer has, best first — what «Настройки» offers to pick from.
///
/// Plain text because the screens draw strings and never an address type: the config crate
/// decides which addresses are worth offering.
pub fn lan_addresses() -> Vec<String> {
    config::first_run::lan_addresses()
        .into_iter()
        .map(|address| address.to_string())
        .collect()
}

fn launch_core_in_unit(launch: &Launch, unit: Option<&str>) -> Result<StartedCore, String> {
    let Some(unit) = unit else {
        return launch_core(launch);
    };
    // Let systemd own the replacement; a detached child would escape its stop/restart policy.
    let output = std::process::Command::new("systemctl")
        .args(["--user", "start", "--", unit])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "systemctl start {unit}: {}",
            short_error(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    Ok(StartedCore {
        program: launch.program.clone(),
        pid: None,
    })
}

/// Starts `bifrost run` detached, with the flags the window was given.
///
/// `docs/03-architecture.md`: «Закрытие окна GUI не влияет на активный сеанс». The child gets
/// no inherited standard streams; a background waiter reaps it without tying its lifetime
/// to the window or blocking the caller.
pub fn launch_core(launch: &Launch) -> Result<StartedCore, String> {
    let program = launch.program.clone();
    let mut command = std::process::Command::new(&program);
    command
        .args(&launch.flags)
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The service is a console program and this window is not: without this it would open a
    // console window of its own, which is the black slab a person would then have to keep
    // alive. Its logs go to `[log]`'s directory (`crates/logging`), not to that console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    match command.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            // As with the editor, keep a waiter rather than leave a zombie after exit.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(StartedCore {
                program,
                pid: Some(pid),
            })
        }
        Err(error) => Err(format!("{}: {error}", program.display())),
    }
}

/// Opens the window's two halves: the state that draws and the client that talks.
pub fn boot(args: CliArgs) -> (ViewModel, GuiClient) {
    boot_with_program(args, own_program())
}

/// [`boot`] with the executable the window starts its core from named explicitly: an
/// integration test is not `bifrost`, so it points at the real binary.
pub fn boot_with_program(args: CliArgs, program: PathBuf) -> (ViewModel, GuiClient) {
    let lang = Lang::from_system();
    let launch = Launch {
        program,
        flags: args.flags(),
    };
    let paths = match args.resolve() {
        Ok(paths) => paths,
        Err(why) => {
            let mut view = ViewModel::new(
                CoreEndpoint::Failed {
                    why: fill_named(
                        lang,
                        Key::CouldNotWorkOutTheDirectoriesWhy,
                        &[("why", &why)],
                    ),
                },
                LocalConfig::default(),
            );
            view.push_log(fill(
                lang,
                Key::CouldNotWorkOutTheDirectories,
                &[&short_error(&why)],
            ));
            return (view, GuiClient::start(None, launch));
        }
    };
    let config = LocalConfig::load(&paths);
    let (endpoint, transport) = resolve_endpoint(&paths);
    let client = GuiClient::start(transport, launch);
    let mut view = ViewModel::new(endpoint, config);
    // The addresses this computer really has: the dropdown of «Настройки» offers them, and
    // the address the file names is added next to them by the screen itself.
    view.settings.lan.candidates = lan_addresses();
    (view, client)
}

// ---------------------------------------------------------------- text helpers

/// `true` for finite values; a non-finite one is replaced, because TOML has no NaN.
fn finite(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// A display's `EndpointView` in the configuration schema; `None` when it names nothing.
fn endpoint_config(view: &ipc::EndpointView) -> Option<config::EndpointConfig> {
    if view.node_id.is_empty() || view.display_id.is_empty() {
        return None;
    }
    let edge = config::Edge::parse(&view.edge).ok()?;
    if view.display_id == config::AUTO_DISPLAY {
        // A desktop anchor carries no segment: the core derives it from the live manifests, and
        // writing one back would be a claim about a monitor the document does not name.
        return Some(config::EndpointConfig {
            node: node_reference(&view.node_id),
            display: config::AUTO_DISPLAY.to_owned(),
            edge,
            start: 0.0,
            end: 0.0,
        });
    }
    let start = finite(view.segment_start, 0.0);
    let end = finite(view.segment_end, start + 1.0);
    let (start, end) = if start < end {
        (start, end)
    } else {
        (start, start + 1.0)
    };
    Some(config::EndpointConfig {
        node: node_reference(&view.node_id),
        display: view.display_id.clone(),
        edge,
        start,
        end,
    })
}

/// A 64-hex node id as `[topology]` writes a peer reference.
///
/// The GUI never claims to know which member is *this* computer unless
/// [`ViewModel::self_node`] can tell, so it names both ends the same way: `"peer:<hex>"`.
/// `config::parse_node_ref` resolves that to the same `NodeId` `"self"` would, on the node
/// that is running this window — which is the node the document is sent to.
fn node_reference(node_id: &str) -> String {
    if node_id.len() == 64 && node_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        format!("peer:{node_id}")
    } else if node_id.is_empty() {
        "self".to_owned()
    } else {
        node_id.to_owned()
    }
}

/// The `author` field of `[topology]`: a node id, or `"self"` when the revision has none.
fn author_reference(author: &str, revision: u64) -> String {
    if author.is_empty() {
        if revision == 0 {
            String::new()
        } else {
            "self".to_owned()
        }
    } else {
        author.to_owned()
    }
}

/// The first 8 characters of a node id, for a label that has to stay short.
pub fn short_id(node_id: &str) -> String {
    node_id.chars().take(8).collect()
}

/// A 64-digit hex node id as its 32 identity bytes; empty when the text is not one.
///
/// This is the form the wire wants (`RevokePeer.peer_node_id`, `UpdateTopology.expected_author`)
/// and the core reads it with `<[u8; 32]>::try_from`, where an empty vector means «no revision
/// yet» (`layout::Pair::NONE`) — so a text that is not a node id becomes that, never a guess.
pub fn node_id_bytes(node_id: &str) -> Vec<u8> {
    let text = node_id.trim();
    if text.len() != 64 {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(32);
    for pair in text.as_bytes().as_chunks::<2>().0 {
        let pair = std::str::from_utf8(pair).unwrap_or("");
        match u8::from_str_radix(pair, 16) {
            Ok(byte) => bytes.push(byte),
            Err(_) => return Vec::new(),
        }
    }
    bytes
}

/// The last 8 characters of a node id, for a link id that has to stay short and unique.
fn short_key(node_id: &str) -> String {
    let len = node_id.chars().count();
    node_id.chars().skip(len.saturating_sub(8)).collect()
}

/// A node id as a sentence fragment.
fn author_label(lang: Lang, author: &str) -> String {
    if author.is_empty() {
        text(lang, Key::Unknown).to_owned()
    } else {
        fill(lang, Key::ComputerNamed, &[&short_id(author)])
    }
}

/// The six-digit code of ADR-010, grouped as «482 913».
pub fn spaced_code(code: &str) -> String {
    let digits: String = code.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != code.chars().count() || digits.len() < 4 {
        return code.to_owned();
    }
    let mut spaced = String::with_capacity(digits.len() + 2);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && index % 3 == 0 {
            spaced.push(' ');
        }
        spaced.push(digit);
    }
    spaced
}

/// The name of a message a GUI should never receive, for a log line.
fn message_name(msg: &Msg) -> &'static str {
    match msg {
        Msg::GetStatus(_) => "GetStatus",
        Msg::GetTopology(_) => "GetTopology",
        Msg::StartPairing(_) => "StartPairing",
        Msg::PairWith(_) => "PairWith",
        Msg::RequestPairing(_) => "RequestPairing",
        Msg::AnswerPairingRequest(_) => "AnswerPairingRequest",
        Msg::CancelPairingRequest(_) => "CancelPairingRequest",
        Msg::SetPairingRequests(_) => "SetPairingRequests",
        Msg::IncomingPairingRequest(_) => "IncomingPairingRequest",
        Msg::OutgoingPairingRequest(_) => "OutgoingPairingRequest",
        Msg::MutePairingRequester(_) => "MutePairingRequester",
        Msg::PairingAnswer(_) => "PairingAnswer",
        Msg::PairingResult(_) => "PairingResult",
        Msg::UpdateTopology(_) => "UpdateTopology",
        Msg::SetClipboard(_) => "SetClipboard",
        Msg::SetClipboardPolicy(_) => "SetClipboardPolicy",
        Msg::SendClipboardHistoryItem(_) => "SendClipboardHistoryItem",
        Msg::SetDiscovery(_) => "SetDiscovery",
        Msg::SetGameGuard(_) => "SetGameGuard",
        Msg::GetClipboardHistory(_) => "GetClipboardHistory",
        Msg::GetClipboardHistoryItem(_) => "GetClipboardHistoryItem",
        Msg::ClearClipboardHistory(_) => "ClearClipboardHistory",
        Msg::ClipboardHistory(_) => "ClipboardHistory",
        Msg::ClipboardHistoryItem(_) => "ClipboardHistoryItem",
        Msg::SetAppearance(_) => "SetAppearance",
        Msg::SetHotkey(_) => "SetHotkey",
        Msg::HotkeysResult(_) => "HotkeysResult",
        Msg::RevokePeer(_) => "RevokePeer",
        Msg::ExportDiagnostics(_) => "ExportDiagnostics",
        Msg::ResetPortalGrants(_) => "ResetPortalGrants",
        Msg::RetryAgent(_) => "RetryAgent",
        Msg::LayoutAuto(_) => "LayoutAuto",
        Msg::SwitchTo(_) => "SwitchTo",
        Msg::Shutdown(_) => "Shutdown",
        Msg::ClaimOwnership(_) => "ClaimOwnership",
        Msg::AbandonOwnership(_) => "AbandonOwnership",
        Msg::ListPendingEdits(_) => "ListPendingEdits",
        Msg::ApplyPendingEdit(_) => "ApplyPendingEdit",
        Msg::DiscardPendingEdit(_) => "DiscardPendingEdit",
        Msg::UpdateAll(_) => "UpdateAll",
        Msg::SetRemoteUpdate(_) => "SetRemoteUpdate",
        Msg::UpdateProgress(_) => "UpdateProgress",
        Msg::UpdateReady(_) => "UpdateReady",
        Msg::Status(_) => "Status",
        Msg::PairingPrompt(_) => "PairingPrompt",
        Msg::OpResult(_) => "OpResult",
        Msg::TopologyDoc(_) => "TopologyDoc",
        Msg::PendingEdits(_) => "PendingEdits",
        Msg::PairingWindow(_) => "PairingWindow",
    }
}

/// What a code means, in a sentence the owner can act on.
fn code_label(lang: Lang, code: &str) -> String {
    match code {
        "" => text(lang, Key::NoCode).to_owned(),
        "EMERGENCY_HOTKEY" => text(lang, Key::NoEmergencyKey).to_owned(),
        "RECEIVER_TAKEOVER" => text(lang, Key::TheSessionCannotBeEndedWithPhysicalInput).to_owned(),
        "TEXT_CLIPBOARD" => text(lang, Key::TheSharedClipboardIsUnavailable2).to_owned(),
        "ABSOLUTE_POINTER" => text(lang, Key::NoAbsolutePointer).to_owned(),
        "CAPTURE" => text(lang, Key::InputCannotBeCaptured).to_owned(),
        "INJECT" => text(lang, Key::EventsCannotBeInjected).to_owned(),
        "LAYOUT_CONFLICT" => text(lang, Key::TheLayoutWasChangedOnAnotherComputer).to_owned(),
        "INVALID_TOPOLOGY" => text(lang, Key::TheLayoutDocumentIsInvalid).to_owned(),
        "NO_COORDINATOR" => text(lang, Key::ThereIsNoCoordinatorOwner).to_owned(),
        "PENDING_VERDICT" => text(lang, Key::TheCoordinatorDidNotConfirmTheEdit).to_owned(),
        "UNKNOWN_EDIT" => text(lang, Key::ThatEditIsGone).to_owned(),
        "UNKNOWN_PEER" => text(lang, Key::ThisComputerIsNotPaired).to_owned(),
        "TRUST_WRITE" => text(lang, Key::CouldNotWriteThePeerList).to_owned(),
        "CLIPBOARD_DISABLED" => text(lang, Key::TheClipboardDidNotTurnOn).to_owned(),
        "EXPORT_FAILED" => text(lang, Key::CouldNotWriteTheDiagnosticsFile).to_owned(),
        "PAIRING_FAILED" => text(lang, Key::ThePairingCouldNotBeAgreed).to_owned(),
        "PAIRING_GONE" => text(lang, Key::ThePairingWindowIsAlreadyClosed).to_owned(),
        "PAIRING_WINDOW_OPEN" => text(lang, Key::ThePairingWindowIsAlreadyOpen).to_owned(),
        "NO_WINDOW" => text(lang, Key::ThePairingWindowIsNotOpen).to_owned(),
        "BAD_ADDRESS" => text(lang, Key::TheAddressIsWrong).to_owned(),
        "UNKNOWN_ATTEMPT" => text(lang, Key::ThisPairingIsNoLongerWaitingForAnAnswer).to_owned(),
        "CLAIM_REFUSED" => text(lang, Key::TakingControlDidNotWork).to_owned(),
        "SWITCH_REFUSED" => text(lang, Key::TheSwitchDidNotWork).to_owned(),
        "PORTAL_STORE" => text(lang, Key::ThePortalsDidNotAnswer).to_owned(),
        "AGENT_LOST" => text(lang, Key::TheInputAgentIsGone).to_owned(),
        "BAD_REQUEST" => text(lang, Key::TheCoreDidNotUnderstandTheRequest).to_owned(),
        other => fill_named(lang, Key::CodeOther, &[("other", &other)]),
    }
}

/// The sentence a successful request is reported with.
fn success_text(lang: Lang, request: Request) -> String {
    match request {
        Request::Claim => text(lang, Key::ThisComputerHasControl).to_owned(),
        Request::Abandon => text(lang, Key::ControlWasGivenBack).to_owned(),
        Request::PairWith => text(lang, Key::Connecting).to_owned(),
        Request::PairingAnswer => text(lang, Key::TheAnswerWasSent).to_owned(),
        Request::PairingRequest => text(lang, Key::TheAnswerWasSent).to_owned(),
        Request::SetPairingRequests => text(lang, Key::PairingRequests).to_owned(),
        Request::UpdateTopology => text(lang, Key::TheLayoutIsSaved).to_owned(),
        Request::UpdateAll => text(lang, Key::UpdatePending).to_owned(),
        Request::SetRemoteUpdate => text(lang, Key::AllowRemoteUpdates).to_owned(),
        Request::RegisterUpdateCoordinator => text(lang, Key::TheCoreAnswered).to_owned(),
        Request::SetClipboard => text(lang, Key::TheClipboardWasSwitched).to_owned(),
        Request::GetClipboardHistory => text(lang, Key::TheClipboardHistoryIsRefreshed).to_owned(),
        Request::CopyClipboardHistory => text(lang, Key::TheTextIsCopied).to_owned(),
        Request::SendClipboardHistory => text(lang, Key::SendClipboardTo).to_owned(),
        Request::SetClipboardPolicy => text(lang, Key::ClipboardRouting).to_owned(),
        Request::ClearClipboardHistory => text(lang, Key::TheClipboardHistoryIsCleared).to_owned(),
        Request::SetAppearance => text(lang, Key::TheEdgeEffectsWereSwitched).to_owned(),
        Request::SetDiscovery => text(lang, Key::LanDiscovery).to_owned(),
        Request::SetGameGuard => text(lang, Key::GameMode).to_owned(),
        Request::SetHotkey => text(lang, Key::KeyboardShortcutSaved).to_owned(),
        Request::RevokePeer => text(lang, Key::TheComputerWasRemovedFromThePair).to_owned(),
        Request::ResetPortalGrants => text(lang, Key::ThePortalGrantsWereForgotten).to_owned(),
        Request::RetryAgent => text(lang, Key::TheInputServiceIsStartingAgain).to_owned(),
        Request::Shutdown => text(lang, Key::TheServiceIsStopping2).to_owned(),
        Request::ExportDiagnostics => text(lang, Key::TheDiagnosticsAreSaved).to_owned(),
        Request::StartPairing => text(lang, Key::ThePairingWindowIsOpen).to_owned(),
        Request::GetStatus | Request::GetTopology => text(lang, Key::TheCoreAnswered).to_owned(),
    }
}

/// The sentence a failed request is reported with.
fn failure_text(lang: Lang, request: Request, code: &str) -> String {
    let label = code_label(lang, code);
    match request {
        Request::UpdateTopology if code == "LAYOUT_CONFLICT" => {
            text(lang, Key::TheLayoutWasChangedOnAnotherComputerDragItAgain).to_owned()
        }
        Request::PairingAnswer if code == "PAIRING_FAILED" => {
            text(lang, Key::TheCodesDidNotMatchOrThePartnerRefused).to_owned()
        }
        _ => label,
    }
}

// ---------------------------------------------------------------- boot helpers

/// The GUI's own arguments, read from the process command line.
pub fn cli_args_from_env() -> CliArgs {
    CliArgs::parse(std::env::args().skip(1))
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn attached_core_restart_sends_shutdown_and_keeps_systemd_supervision() {
        let root =
            std::env::temp_dir().join(format!("bifrost-restart-probe-{}", std::process::id()));
        let paths = CliArgs::parse(["--root".to_owned(), root.display().to_string()])
            .resolve()
            .unwrap();
        let view = ViewModel::new(
            CoreEndpoint::Ready {
                description: "test endpoint".into(),
            },
            LocalConfig {
                paths: Some(paths),
                ..Default::default()
            },
        );
        let (commands, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let (_events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = GuiClient {
            commands: Some(commands),
            events: receiver,
            thread: None,
            wake: std::sync::Arc::new(std::sync::OnceLock::new()),
        };
        let mut bridge = Bridge::new(view, client);
        bridge.view.on_connected("test endpoint");
        bridge.core_unit = Some("app-bifrost@fixture.service".into());
        let now = Instant::now();
        bridge.restart_core(now);
        assert!(matches!(requests.try_recv().unwrap(),
            Command::Send { message, .. } if matches!(*message, Msg::Shutdown(_))));
        assert!(
            requests.try_recv().is_err(),
            "no immediate process launch or termination"
        );
        bridge.poll_restart(now);
        assert!(
            requests.try_recv().is_err(),
            "process exit must also wait for the client's old-link disconnect event"
        );
        bridge
            .view
            .on_disconnected("old core closed the link".into());
        assert!(
            requests.try_recv().is_err(),
            "link loss alone must not start a core"
        );
        // These isolated, nonexistent endpoints are now free. No real service is touched.
        bridge.poll_restart(now);
        assert!(matches!(requests.try_recv().unwrap(),
            Command::StartCore(Some(unit)) if unit == "app-bifrost@fixture.service"));
        assert!(
            requests.try_recv().is_err(),
            "one supervised start, never a detached second core"
        );

        // Even a client that never reports EOF must not leave the restart spinning.
        bridge.view.settings.service.restart = None;
        bridge.view.on_connected("test endpoint");
        bridge.restart_core(now);
        assert!(matches!(requests.try_recv().unwrap(), Command::Send { .. }));
        bridge.poll_restart(now + crate::core_exit::TIMEOUT);
        assert!(
            requests.try_recv().is_err(),
            "timeout never launches a duplicate"
        );
        assert!(bridge.view.settings.service.restart.is_none());
        assert!(!bridge.view.settings.service.line.as_ref().unwrap().ok);
    }

    #[tokio::test]
    async fn systemd_reconnection_before_queued_start_is_success_not_a_second_launch() {
        let (stream, _peer) = tokio::io::duplex(128);
        let mut link = ::ipc::GuiLink::new(stream);
        let (commands, mut requests) = tokio::sync::mpsc::unbounded_channel();
        commands
            .send(Command::StartCore(Some("fixture.service".into())))
            .unwrap();
        drop(commands);
        let (tx, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let events = Events {
            tx,
            wake: std::sync::Arc::new(std::sync::OnceLock::new()),
        };
        assert!(serve(&mut link, &mut requests, &events).await.is_none());
        assert!(matches!(
            receiver.try_recv().unwrap(),
            GuiEvent::CoreStarted {
                started: Ok(StartedCore { pid: None, .. })
            }
        ));
        assert!(receiver.try_recv().is_err());
    }
}
