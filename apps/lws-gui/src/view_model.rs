//! Everything the window decides, and nothing about how it looks.
//!
//! The split this crate is built around (see the crate docs): [the view model](ViewModel)
//! is plain data with no `egui` type anywhere near it, [the screens](crate::screens) turn it
//! into widgets and report what the user asked for as [`Action`]s, and [`Bridge`] is the only
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
/// (`GetStatus`, `ClaimOwnership`, `StartPairing`, …), the two things the client does by
/// itself (`StartCore`, `ConnectAgain`), and the local edits the window applies to its own
/// state (`ShowScreen`, `SetSide`, `MoveDisplay`, …). Only the first group is a `GuiFrame`;
/// the app sends those to the client and folds the rest into the view model, so no decision
/// ever happens in drawing code. `StopCore` asks the core to stop itself, so it also works for
/// a service started elsewhere; only a timed-out request may use the pid this window started.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ::ipc::transport::Endpoint;
use config::Paths;
// `protocol::ipc` and the `ipc` crate are two different things, and this module needs both:
// `ipc::` is the wire vocabulary, and the local transport is named with a leading `::`.
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

/// How long a request may stay unanswered before the window says «Служба не отвечает».
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// How many lines of the shared log the window keeps in front of the user.
pub const LOG_LINES: usize = 20;

/// How long the client waits before it tries the core again.
///
/// The build spec asks for a 2 s retry, so a core started by hand (or by «Запустить» on
/// another desktop) makes the window come alive on its own.
pub const RETRY_DELAY: Duration = Duration::from_secs(2);

/// How long the GUI waits for the core to stop itself before using its owned pid or reporting failure.
pub const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The oldest requests kept while they wait for an answer.
const INFLIGHT_LIMIT: usize = 32;

// ---------------------------------------------------------------- small types

/// One of the four screens of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// «Статус» — the default screen.
    Status,
    /// «Сопряжение».
    Pairing,
    /// «Экраны» — the layout editor (KVM-077).
    Layout,
    /// «Настройки».
    Settings,
}

impl Screen {
    /// Every screen, in the order the tab bar shows them.
    pub const ALL: [Screen; 4] = [
        Screen::Status,
        Screen::Pairing,
        Screen::Layout,
        Screen::Settings,
    ];

    /// The tab title.
    pub const fn title(self) -> &'static str {
        match self {
            Screen::Status => "Статус",
            Screen::Pairing => "Сопряжение",
            Screen::Layout => "Экраны",
            Screen::Settings => "Настройки",
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
    /// Every side, in the order the radio buttons show them.
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    /// The button label.
    pub const fn label(self) -> &'static str {
        match self {
            Side::Left => "Слева",
            Side::Right => "Справа",
            Side::Top => "Сверху",
            Side::Bottom => "Снизу",
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
    /// `PairWith`.
    PairWith,
    /// `PairingAnswer`.
    PairingAnswer,
    /// `UpdateTopology`.
    UpdateTopology,
    /// `SetClipboard`.
    SetClipboard,
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
    /// `LayoutAuto`: rebuild the shared layout from this desktop.
    LayoutAuto,
    /// `ListPendingEdits`.
    ListPendingEdits,
    /// `ApplyPendingEdit`.
    ApplyPendingEdit,
    /// `DiscardPendingEdit`.
    DiscardPendingEdit,
}

impl Request {
    /// The request's name in a log line.
    pub const fn label(self) -> &'static str {
        match self {
            Request::GetStatus => "состояние",
            Request::GetTopology => "раскладка",
            Request::Claim => "взять управление",
            Request::Abandon => "отдать управление",
            Request::StartPairing => "открыть окно сопряжения",
            Request::PairWith => "подключиться по адресу",
            Request::PairingAnswer => "ответ на сопряжение",
            Request::UpdateTopology => "применить раскладку",
            Request::SetClipboard => "буфер обмена",
            Request::RevokePeer => "удалить компьютер из пары",
            Request::ExportDiagnostics => "сохранить диагностику",
            Request::ResetPortalGrants => "забыть разрешения порталов",
            Request::RetryAgent => "повторить запуск службы ввода",
            Request::Shutdown => "остановить службу",
            Request::LayoutAuto => "расположить автоматически",
            Request::ListPendingEdits => "список непринятых правок",
            Request::ApplyPendingEdit => "применить непринятую правку",
            Request::DiscardPendingEdit => "отбросить непринятую правку",
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
    pub fn line(&self) -> String {
        if self.ok {
            format!("Готово: {}", self.text)
        } else {
            format!("Не получилось: {}", self.text)
        }
    }
}

// ---------------------------------------------------------------- local config

/// The settings the window shows, read from `config.toml`.
///
/// Reading the file is what §5.4 of the build spec asks for and the only reason the GUI
/// opens it: the hotkeys are shown, never written, and a missing or broken file falls back
/// to the documented defaults with a line saying so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    /// `[node].display_name`.
    pub display_name: String,
    /// `[input].switch_hotkey`.
    pub switch_hotkey: String,
    /// `[workspace].claim_hotkey`.
    pub claim_hotkey: String,
    /// `[network].listen_address` — «Адрес этого компьютера в локальной сети».
    pub listen_address: String,
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
            switch_hotkey: "Ctrl+Alt+Shift+Right".to_owned(),
            claim_hotkey: "Ctrl+Alt+Shift+F11".to_owned(),
            listen_address: String::new(),
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
                config.switch_hotkey = loaded.input.switch_hotkey.clone();
                config.claim_hotkey = loaded.workspace.claim_hotkey.clone();
                config.listen_address = loaded.network.listen_address.clone();
            }
            Err(error) => {
                config.note = Some(format!(
                    "Настройки не прочитаны ({}), показаны стандартные значения",
                    crate::view_model::short_error(&error.to_string())
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
        }
    }

    /// Move the display by a delta in its own node's logical units.
    fn shift(&mut self, dx: f64, dy: f64) {
        self.x = finite(self.x + dx, self.x);
        self.y = finite(self.y + dy, self.y);
    }
}

/// One member of the workspace, as the editor draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorNode {
    /// The node id, lower-case hex — the form both the wire and `[topology]` use.
    pub node_id: String,
    /// The display name, empty when the core knew none.
    pub display_name: String,
    /// Advisory, as `NodeDisplays.online`.
    pub online: bool,
    /// The member's displays.
    pub displays: Vec<EditorDisplay>,
}

impl EditorNode {
    /// The name to show; a node without one is named by its node id.
    pub fn label(&self) -> String {
        if self.display_name.is_empty() {
            format!("компьютер {}", short_id(&self.node_id))
        } else {
            self.display_name.clone()
        }
    }

    /// The display the editor links through: the primary one, else the first.
    fn main_display(&self) -> Option<&EditorDisplay> {
        self.displays
            .iter()
            .find(|display| display.primary)
            .or_else(|| self.displays.first())
    }

    /// The bounding box of every display, `None` when there is none.
    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let mut bounds: Option<(f64, f64, f64, f64)> = None;
        for display in &self.displays {
            bounds = Some(match bounds {
                None => (
                    display.x,
                    display.y,
                    display.x + display.w,
                    display.y + display.h,
                ),
                Some((min_x, min_y, max_x, max_y)) => (
                    min_x.min(display.x),
                    min_y.min(display.y),
                    max_x.max(display.x + display.w),
                    max_y.max(display.y + display.h),
                ),
            });
        }
        bounds
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
    /// Which side of this computer the other one is on.
    pub side: Side,
    /// The node id the user named as «этот компьютер», when the window cannot tell.
    pub self_choice: Option<String>,
    /// The node id the user named as «другой компьютер».
    pub other_choice: Option<String>,
}

impl LayoutEditor {
    /// `true` once a document has been received.
    pub fn has_document(&self) -> bool {
        !self.nodes.is_empty()
    }
}

// ---------------------------------------------------------------- view model

/// One request waiting for its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inflight {
    /// The `request_id` the frame carried.
    pub id: u64,
    /// What was asked.
    pub request: Request,
    /// When the window stops believing in an answer.
    pub deadline: Instant,
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
    /// Set when the core answered `CLIPBOARD_DISABLED`.
    pub clipboard_note: Option<String>,
    /// The member «Удалить компьютер из пары» is aimed at.
    pub revoke: Option<String>,
    /// `true` once the user pressed the button and the window asks again.
    pub revoke_confirmed: bool,
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
    /// When «Остановить службу» was sent through the core's graceful shutdown request.
    pub stopping: Option<Instant>,
    /// What the last service operation answered, in words.
    pub line: Option<Outcome>,
}

/// What «Перезапустить службу» is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restart {
    /// The stop was sent to the pid this window holds; the window waits for the link to
    /// drop, which is how it knows the old core is really gone.
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
    /// Stop this pid, then wait for the link to drop before anything else.
    Stop(u32),
    /// There is nothing to wait for: ask the client for a fresh core right now.
    Start,
    /// The window must not act; why is already in [`Service::line`].
    Refused,
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
    /// The screen being drawn.
    pub screen: Screen,
    /// The last `Status`, kept across a disconnect so the window is not blank.
    pub status: Option<ipc::Status>,
    /// The editor's model.
    pub editor: LayoutEditor,
    /// The pairing screen's state.
    pub pairing: Pairing,
    /// The divergent edits the core kept after a partition.
    pub pending_edits: Vec<ipc::PendingEdit>,
    /// The settings screen's state.
    pub settings: Settings,
    /// The last finished operation.
    pub outcome: Option<Outcome>,
    /// The `LAYOUT_CONFLICT` the core reported, until the document is refreshed.
    pub conflict: Option<String>,
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
    /// A window that knows where the core is and nothing else.
    pub fn new(endpoint: CoreEndpoint, config: LocalConfig) -> Self {
        let mut view = ViewModel {
            endpoint,
            connection: Connection::Disconnected {
                why: "служба ещё не отвечала".to_owned(),
            },
            core: None,
            config,
            screen: Screen::Status,
            status: None,
            editor: LayoutEditor::default(),
            pairing: Pairing::default(),
            pending_edits: Vec::new(),
            settings: Settings::default(),
            outcome: None,
            conflict: None,
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

    /// The link is gone; the last `Status` stays on screen.
    ///
    /// Returns `true` while «Перезапустить службу» waits for the core it stopped: a dropped
    /// link is the proof it was waiting for, and the window then starts the fresh one.
    pub fn on_disconnected(&mut self, why: String) -> bool {
        self.push_log(format!("Связь со службой потеряна: {}", short_error(&why)));
        self.connection = Connection::Disconnected { why };
        self.inflight.clear();
        self.silent = false;
        // The banner described what the *core we just lost* did («Готово: …»). Leaving it up
        // under a header that now says «Служба не запущена» is the window contradicting
        // itself, so it goes with the connection (batch-8 audit).
        self.outcome = None;
        // A pairing window belongs to the connection that opened it; the core closes it on
        // its own timeout, and pretending it is still counting down would be a lie.
        self.pairing.window = None;
        self.pairing.prompt = None;
        if self.settings.service.stopping.take().is_some() {
            self.settings.service.line = Some(Outcome::done("Служба остановлена"));
            return false;
        }
        if matches!(
            self.settings.service.restart,
            Some(Restart::Stopping { .. })
        ) {
            self.settings.service.restart = Some(Restart::Starting);
            return true;
        }
        false
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

    /// Records a request that was just handed to the client.
    pub fn begin_request(&mut self, request: Request, now: Instant) -> u64 {
        self.next_request_id += 1;
        let id = self.next_request_id;
        self.inflight.push(Inflight {
            id,
            request,
            deadline: now + REQUEST_TIMEOUT,
        });
        if self.inflight.len() > INFLIGHT_LIMIT {
            self.inflight.remove(0);
        }
        id
    }

    /// The client could not put a request on the wire (no core is listening).
    pub fn on_not_sent(&mut self, id: u64) {
        let request = self.take_inflight(id);
        let name = request.map_or("запрос", Request::label);
        self.push_log(format!("Служба не запущена: {name} не отправлено"));
        if request == Some(Request::Shutdown) {
            self.settings.service.stopping = None;
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

    /// Ages the deadlines; called once per frame.
    pub fn tick(&mut self, now: Instant) {
        self.silent = self
            .inflight
            .iter()
            .any(|inflight| now >= inflight.deadline);
        // «Перезапустить службу» waits for the link to drop, and that wait has an end: past
        // it the service is left to the user rather than watched from a spinner, because
        // that is the only thing left that can be said honestly.
        let seconds = SERVICE_STOP_TIMEOUT.as_secs();
        let stalled = matches!(
            &self.settings.service.restart,
            Some(Restart::Stopping { since }) if now.duration_since(*since) >= SERVICE_STOP_TIMEOUT
        );
        if stalled {
            self.settings.service.restart = None;
            self.push_log("Служба не остановилась");
            self.settings.service.line = Some(Outcome::failed(format!(
                "служба не остановилась за {seconds} c — остановите её сами и нажмите «Перезапустить службу» снова"
            )));
        }
    }

    /// The core process was started, or could not be.
    pub fn on_core_started(&mut self, started: Result<StartedCore, String>) {
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
                self.push_log(format!(
                    "Не удалось запустить службу: {}",
                    short_error(&why)
                ));
                self.settings.service.restart = None;
                self.outcome = Some(Outcome::failed(format!(
                    "не удалось запустить службу ({})",
                    short_error(&why)
                )));
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
        // `TopologyDoc`, `PendingEdits`) and still carries the `request_id` it answers, so
        // those requests are retired here; the write half answers with an `OpResult`, which
        // is the one message that has to name its request to be understood.
        if !matches!(&msg, Msg::OpResult(_)) {
            self.take_inflight(frame.request_id);
        }
        match msg {
            Msg::Status(status) => self.apply_status(status),
            Msg::TopologyDoc(doc) => self.apply_topology(doc),
            Msg::PendingEdits(edits) => {
                let count = edits.edits.len();
                self.pending_edits = edits.edits;
                self.push_log(format!("Непринятых правок раскладки: {count}"));
            }
            Msg::PairingWindow(window) => self.apply_pairing_window(window),
            Msg::PairingPrompt(prompt) => {
                self.push_log(format!(
                    "Сопряжение: партнёр {} предлагает сверить код",
                    if prompt.peer_name.is_empty() {
                        "без имени"
                    } else {
                        prompt.peer_name.as_str()
                    }
                ));
                self.pairing.details = false;
                self.pairing.prompt = Some(prompt);
            }
            Msg::OpResult(result) => self.apply_op_result(frame.request_id, result),
            // The pushed half of the contract never arrives here; a GUI that gets one of its
            // own requests back is talking to something that is not a core.
            other => self.push_log(format!(
                "Служба прислала неожиданный кадр ({})",
                message_name(&other)
            )),
        }
    }

    /// `Status` is a response and an event, and both are the same fold.
    fn apply_status(&mut self, status: ipc::Status) {
        if self
            .status
            .as_ref()
            .is_none_or(|old| old.layout_revision != status.layout_revision)
        {
            self.push_log(format!(
                "Раскладка: ревизия {} от {}",
                status.layout_revision,
                author_label(&status.layout_author)
            ));
        }
        self.editor.editable = status.layout_editable;
        // The editor follows the core: what it says is applied is what the editor holds, so
        // a revision applied elsewhere moves the base the next «Применить» names.
        if status.layout_revision > self.editor.revision {
            self.editor.revision = status.layout_revision;
            self.editor.author = status.layout_author.clone();
        }
        self.status = Some(status);
    }

    /// A whole document: the editor's base, as long as it is not older than the one held.
    fn apply_topology(&mut self, doc: ipc::TopologyDoc) {
        if doc.revision < self.editor.revision {
            self.push_log(format!(
                "Раскладка: ревизия {} старше текущей {} — оставлена прежняя",
                doc.revision, self.editor.revision
            ));
            return;
        }
        self.conflict = None;
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
                online: member.online,
                displays: member
                    .displays
                    .iter()
                    .map(EditorDisplay::from_view)
                    .collect(),
            })
            .collect();
        // A dragged display belongs to a node and a display id; a new document replaces the
        // positions wholesale, because the applied revision is the truth about them.
        self.editor.nodes = nodes;
        self.editor.self_choice = None;
        self.editor.other_choice = None;
        // The applied document places both computers at their own origins, which draws one on
        // top of the other. The window's own arrangement is the pair at the chosen edge, so
        // the preview equals what «Применить» would send from the first frame on.
        self.place_other(self.editor.side);
        self.push_log(format!(
            "Раскладка получена: ревизия {} от {}, участников {}",
            doc.revision,
            author_label(&doc.author_node_id),
            doc.members.len()
        ));
    }

    /// The pairing window opened, changed what is left of it, or closed.
    fn apply_pairing_window(&mut self, window: ipc::PairingWindow) {
        if window.open {
            self.push_log(format!(
                "Окно сопряжения открыто: {} ({} с)",
                window.address, window.seconds_remaining
            ));
            self.pairing.window = Some(window);
        } else {
            self.push_log("Окно сопряжения закрыто");
            self.pairing.window = None;
            self.pairing.prompt = None;
        }
    }

    /// The verdict of one request.
    fn apply_op_result(&mut self, request_id: u64, result: ipc::OpResult) {
        let Some(request) = self.take_inflight(request_id) else {
            self.push_log(format!(
                "Ответ на неизвестный запрос ({})",
                code_label(&result.error_code)
            ));
            return;
        };
        if result.ok {
            let text = self.success_text(request);
            self.outcome = Some(Outcome::done(text.clone()));
            if request == Request::Shutdown {
                self.settings.service.line = Some(Outcome::done("Служба останавливается"));
            }
            self.push_log(format!("{}: готово ({text})", request.label()));
            return;
        }
        let code = result.error_code.as_str();
        let failure = failure_text(request, code);
        self.push_log(format!("{}: {}", request.label(), failure));
        if request == Request::SetClipboard && code == "CLIPBOARD_DISABLED" {
            self.settings.clipboard_note =
                Some("Буфер обмена выключен в настройках (config.toml)".to_owned());
        }
        if request == Request::UpdateTopology && code == "LAYOUT_CONFLICT" {
            self.conflict = Some(
                "Раскладку изменили на другом компьютере. Обновите документ и повторите."
                    .to_owned(),
            );
        }
        if request == Request::Shutdown {
            self.settings.service.stopping = None;
            self.settings.service.line = Some(Outcome::failed(failure.clone()));
        }
        if request == Request::ExportDiagnostics {
            self.settings.export = Some(Outcome::failed(failure.clone()));
        }
        self.outcome = Some(Outcome::failed(failure));
    }

    /// Removes and returns the request `id` names.
    fn take_inflight(&mut self, id: u64) -> Option<Request> {
        let index = self
            .inflight
            .iter()
            .position(|inflight| inflight.id == id)?;
        Some(self.inflight.remove(index).request)
    }

    /// The sentence a successful request is reported with.
    ///
    /// The pairing answer names the peer; the live link status arrives separately in `Status`.
    fn success_text(&self, request: Request) -> String {
        if request == Request::LayoutAuto {
            // The owner asked for this in one sentence: what the automatic layout did.
            return match self.auto_layout_summary() {
                Some(summary) => format!("расположено автоматически: {summary}"),
                None => "расположено автоматически".to_owned(),
            };
        }
        if request == Request::PairingAnswer {
            let name = self
                .pairing
                .prompt
                .as_ref()
                .map(|prompt| prompt.peer_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Компьютер".to_owned());
            return format!("{name} сопряжён");
        }
        success_text(request)
    }

    // ------------------------------------------------------------ derived text

    /// What the automatic layout produced, for the status line: which computer it placed,
    /// on which side, and that the whole desktop is part of it.
    ///
    /// Derived from the applied document the window already holds (the auto layout is one
    /// current node plus the peer, side by side), so the sentence describes what happened
    /// rather than what was asked for.
    fn auto_layout_summary(&self) -> Option<String> {
        let other = self.other_node()?;
        let name = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == other)
            .map(|node| node.label())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "второй компьютер".to_owned());
        let side = match self.editor.side {
            Side::Left => "слева",
            Side::Right => "справа",
            Side::Top => "сверху",
            Side::Bottom => "снизу",
        };
        Some(format!("{name} {side}, весь рабочий стол"))
    }

    /// The header's status line, in the words of someone who did not install anything: the
    /// thing that must be running is «служба», not «ядро».
    pub fn connection_label(&self) -> &'static str {
        if self.silent {
            "Служба не отвечает"
        } else if self.connection.is_connected() {
            "Служба работает"
        } else {
            "Служба не запущена"
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
        let Some(status) = &self.status else {
            return "Нет данных от службы".to_owned();
        };
        match status.focus.as_str() {
            "LOCAL" => "Ввод здесь".to_owned(),
            "PREPARING" => "Готовим переход…".to_owned(),
            "REMOTE" => {
                let peer = if status.active_peer.is_empty() {
                    "другой компьютер".to_owned()
                } else {
                    status.active_peer.clone()
                };
                format!("Ввод на {peer}")
            }
            "BLOCKED" => "Ввод остановлен из-за ошибки".to_owned(),
            other => format!("Ввод: служба сообщила неизвестное состояние ({other})"),
        }
    }

    /// The small line under [`ViewModel::focus_line`]: the one fact that does not fit the
    /// headline, which is the keyboard layout in use on the other computer.
    pub fn focus_detail(&self) -> Option<String> {
        let status = self.status.as_ref()?;
        (status.focus == "REMOTE" && !status.active_layout.is_empty())
            .then(|| format!("раскладка {}", status.active_layout))
    }

    /// Whether the link to the other computer is up.
    pub fn link_line(&self) -> &'static str {
        match &self.status {
            Some(status) if status.connected => "Есть связь",
            Some(_) => "Нет связи",
            None => "Нет данных от службы",
        }
    }

    /// The links the core marked inactive because one of their displays is not in the live
    /// manifests, as plain lines for the GUI. Empty when every link is usable.
    ///
    /// The link is not lost: it becomes active again on its own once the monitor is reported.
    pub fn inactive_link_lines(&self) -> Vec<String> {
        self.editor
            .links
            .iter()
            .filter(|link| !link.active)
            .map(|link| {
                if link.inactive_reason.is_empty() {
                    format!("Связь {} неактивна: экран не найден", link.id)
                } else {
                    format!(
                        "Связь {} неактивна: экран «{}» не найден",
                        link.id, link.inactive_reason
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
                    "экраны ещё не получены".to_owned()
                } else {
                    link.auto_displays.join(", ")
                };
                Some(format!(
                    "Связь {}: Весь рабочий стол (авто) — {screens}",
                    link.id
                ))
            })
            .collect()
    }

    /// One line per **other** computer, or the sentence that says there are none.
    ///
    /// This computer is not a peer of itself: listing it here read as a third machine and the
    /// owner asked why. Who controls the keyboard is said in plain words, «владелец» being
    /// the core's word and not the owner's.
    pub fn member_lines(&self) -> Vec<String> {
        let Some(status) = &self.status else {
            return vec!["Нет данных от службы".to_owned()];
        };
        let this = self.self_node().map(str::to_owned);
        let others: Vec<&protocol::ipc::MemberStatus> = status
            .members
            .iter()
            .filter(|member| !self.is_this_computer(member, this.as_deref()))
            .collect();
        if others.is_empty() {
            return vec!["Пока никого: откройте окно сопряжения".to_owned()];
        }
        others
            .into_iter()
            .map(|member| {
                let name = if member.display_name.is_empty() {
                    format!("компьютер {}", short_id(&member.node_id))
                } else {
                    member.display_name.clone()
                };
                let state = if member.online {
                    "в сети"
                } else {
                    "не в сети"
                };
                let keyboard = if member.is_owner {
                    "управляет клавиатурой"
                } else {
                    "не управляет клавиатурой"
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

    /// The backend in the words the owner reads.
    pub fn backend_label(&self) -> String {
        let Some(status) = &self.status else {
            return "неизвестно".to_owned();
        };
        match status.backend.as_str() {
            "platform-win" => "Windows".to_owned(),
            "platform-x11" => "X11 (Xorg)".to_owned(),
            "platform-wayland" => "Wayland".to_owned(),
            "" => "неизвестно".to_owned(),
            other => other.to_owned(),
        }
    }

    /// The short id of this computer, when the `Status` names it.
    ///
    /// `Status` has no field for «this node»: `owner_node_id` is the owner's id, and it only
    /// means this computer while this computer is the owner. The build spec is explicit
    /// («не выдумывайте id»), so anything else shows the display name alone.
    pub fn short_id_of_self(&self) -> Option<String> {
        let status = self.status.as_ref()?;
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
        let Some(status) = &self.status else {
            return ("нет данных от службы".to_owned(), String::new());
        };
        // The serial is the one number in this block and the build spec asks for it small;
        // it is shown whoever owns input, because it is what a support question is about.
        let serial = if status.owner_serial == 0 {
            String::new()
        } else {
            format!("сеанс владельца {}", status.owner_serial)
        };
        if status.is_owner {
            return ("Этот компьютер".to_owned(), serial);
        }
        let name = status
            .members
            .iter()
            .find(|member| member.is_owner)
            .map(|member| {
                if member.display_name.is_empty() {
                    format!("компьютер {}", short_id(&member.node_id))
                } else {
                    member.display_name.clone()
                }
            })
            .or_else(|| {
                if status.owner_node_id.is_empty() {
                    None
                } else {
                    Some(format!("компьютер {}", short_id(&status.owner_node_id)))
                }
            })
            .unwrap_or_else(|| "нет владельца".to_owned());
        (name, serial)
    }

    /// Why the core reports degraded input service, or `None` while it is healthy.
    pub fn agent_degraded_reason(&self) -> Option<&str> {
        let status = self.status.as_ref()?;
        if !status.agent_degraded_reason.is_empty() {
            Some(status.agent_degraded_reason.as_str())
        } else if status.agent_restart_circuit_open {
            Some("Служба ввода остановлена")
        } else {
            None
        }
    }

    /// The «Ограничения и деградации» block (KVM-056): never invented, only `Status`.
    pub fn limits(&self) -> Vec<String> {
        let Some(status) = &self.status else {
            return vec!["Нет данных от службы".to_owned()];
        };
        let mut lines = Vec::new();
        for code in &status.capabilities_unavailable {
            lines.push(match code.as_str() {
                "EMERGENCY_HOTKEY" => "Нет аварийной клавиши: прервать захват клавиатуры на этом компьютере можно только мышью".to_owned(),
                "RECEIVER_TAKEOVER" => "Физический ввод на этом компьютере не завершает сеанс автоматически — переключение только с источника".to_owned(),
                "TEXT_CLIPBOARD" => "Общий буфер обмена недоступен".to_owned(),
                "ABSOLUTE_POINTER" => "Этот компьютер не принимает абсолютное положение курсора".to_owned(),
                "CAPTURE" => "Этот компьютер не может перехватывать ввод".to_owned(),
                "INJECT" => "Этот компьютер не может вводить события".to_owned(),
                other => format!("Ограничение: {other}"),
            });
        }
        match status.portal_state.as_str() {
            "restoring" => lines.push(
                "Разрешение портала восстанавливается из сохранённого токена; диалог не ожидается"
                    .to_owned(),
            ),
            "permission_required" => lines.push(
                "Ожидается диалог разрешения портала — подтвердите его, иначе узел не сможет принять ввод"
                    .to_owned(),
            ),
            "denied" => lines.push(
                "Доступ отклонён в диалоге разрешения. Нажмите «Забыть разрешения порталов» в настройках и попробуйте снова"
                    .to_owned(),
            ),
            "unavailable" => lines.push("Порталы рабочего стола недоступны на этой системе".to_owned()),
            _ => {}
        }
        if status.backend == "platform-wayland" {
            // Only what Wayland as a platform means. The rest of what it costs — the
            // missing takeover, the emergency hotkey — arrives as the capability codes
            // above, if the core could not provide them: on Wayland where they *are*
            // provided (a desktop that supports them), this line must not claim otherwise.
            lines.push(
                "Wayland: перехват ввода возможен только через порталы рабочего стола".to_owned(),
            );
        }
        if !status.last_error_code.is_empty() {
            lines.push(format!(
                "Последнее сообщение: {}",
                code_label(&status.last_error_code)
            ));
        }
        lines
    }

    /// The latency line, or `None` while nobody measures it.
    pub fn delay_line(&self) -> Option<String> {
        let status = self.status.as_ref()?;
        if status.rtt_p95_us == 0 {
            return None;
        }
        Some(format!("Задержка: {} мс", status.rtt_p95_us / 1_000))
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
    /// `Status` carries no «this node» field, so the window uses what it does carry: while
    /// this node is the owner, `owner_node_id` is itself; otherwise, with two members and
    /// exactly one of them marked as the owner, the other one is this computer. Anything
    /// else — three members, no owner known — leaves the question to the user, who picks
    /// with [`Action::ChooseSelf`].
    pub fn self_node(&self) -> Option<&str> {
        if let Some(choice) = self.editor.self_choice.as_deref() {
            return Some(choice);
        }
        let status = self.status.as_ref()?;
        let known = |node_id: &str| self.editor.nodes.iter().any(|node| node.node_id == node_id);
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

    /// The other member the editor links this computer to.
    pub fn other_node(&self) -> Option<&str> {
        if let Some(choice) = self.editor.other_choice.as_deref() {
            return Some(choice);
        }
        let this = self.self_node()?;
        self.editor
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .find(|node_id| *node_id != this)
    }

    /// The stretch of edge the link between this computer and the other one uses, as
    /// `(my display, my edge, their display, their edge, end)` in logical units, with
    /// `start` always `0.0` — the same span [`ViewModel::pair_link`] writes into the
    /// document, so the picture cannot disagree with what «Применить» sends.
    pub fn linked_span(&self) -> Option<(String, config::Edge, String, config::Edge, f64)> {
        let (this, other) = (self.self_node()?, self.other_node()?);
        let mine = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == this)?
            .main_display()?;
        let theirs = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == other)?
            .main_display()?;
        let (my_edge, their_edge) = self.editor.side.edges();
        let (my_extent, their_extent) = if self.editor.side.horizontal() {
            (mine.h, theirs.h)
        } else {
            (mine.w, theirs.w)
        };
        let end = edge_span(my_extent, their_extent);
        Some((
            mine.id.clone(),
            my_edge,
            theirs.id.clone(),
            their_edge,
            if end > 0.0 { end } else { 1.0 },
        ))
    }

    /// The edited document as `[topology]` TOML — what `UpdateTopology` carries.
    ///
    /// The schema is the one `crates/config` defines for `[topology]`
    /// (`docs/config.example.toml`), so the core parses it as
    /// `config::TopologyConfig` and the file this text would be a section of parses as
    /// `config::Config`: `revision > 0` needs an author, the floor never trails the applied
    /// revision, a link id is unique, an endpoint is `"self"` or `"peer:<64 hex digits>"`,
    /// and every segment is finite with `0 <= start < end`.
    pub fn layout_toml(&self) -> String {
        let topology = self.topology_config();
        // Only a non-finite number can get here, and the fields are sanitised on the way in;
        // an empty document is refused by the core with a sentence the user can read, which
        // beats panicking in a status window.
        toml::to_string(&topology).unwrap_or_default()
    }

    /// The editor's model in the configuration schema.
    fn topology_config(&self) -> config::TopologyConfig {
        let mut topology = config::TopologyConfig {
            revision: self.editor.revision,
            author: author_reference(&self.editor.author, self.editor.revision),
            max_seen_revision: self.editor.revision,
            max_seen_author: author_reference(&self.editor.author, self.editor.revision),
            displays: Vec::new(),
            links: Vec::new(),
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
        topology.links = self.editor_links();
        topology
    }

    /// The links of the edited document: the applied ones, plus the one the user is making.
    fn editor_links(&self) -> Vec<config::LinkConfig> {
        let mut links: Vec<config::LinkConfig> = Vec::new();
        let (Some(this), Some(other)) = (self.self_node(), self.other_node()) else {
            // Without a pair to place there is nothing to derive; the applied links stay as
            // they are rather than being dropped from a document that is about to be applied.
            return self.applied_links(None);
        };
        let pair = [this.to_owned(), other.to_owned()];
        links.extend(self.applied_links(Some(&pair)));
        if let Some(link) = self.pair_link(this, other) {
            if !links.iter().any(|existing| existing.id == link.id) {
                links.push(link);
            }
        }
        links
    }

    /// The applied links, minus the ones the edited pair replaces.
    fn applied_links(&self, pair: Option<&[String; 2]>) -> Vec<config::LinkConfig> {
        self.editor
            .links
            .iter()
            .enumerate()
            .filter_map(|(index, link)| {
                let from = link.from.as_ref()?;
                let to = link.to.as_ref()?;
                if let Some(pair) = pair {
                    let touches = |node_id: &str| pair.iter().any(|member| member == node_id);
                    if touches(&from.node_id) || touches(&to.node_id) {
                        return None;
                    }
                }
                Some(config::LinkConfig {
                    id: if link.id.is_empty() {
                        format!("link-{index}")
                    } else {
                        link.id.clone()
                    },
                    from: endpoint_config(from)?,
                    to: endpoint_config(to)?,
                    bidirectional: link.bidirectional,
                })
            })
            .collect()
    }

    /// The link between this computer and the other one, as the user placed them.
    ///
    /// Choosing a side («ноутбук справа») is a statement about the two *desktops*, so both
    /// endpoints are desktop anchors (`display = "auto"`): the link then follows the monitors —
    /// one switched off, one added back, a rotation — with no further edit, which is the whole
    /// point of not pinning displays in a document. The segment is not written for an anchor:
    /// the core derives it from the live manifests.
    fn pair_link(&self, this: &str, other: &str) -> Option<config::LinkConfig> {
        self.editor.nodes.iter().find(|node| node.node_id == this)?;
        self.editor
            .nodes
            .iter()
            .find(|node| node.node_id == other)?;
        let side = self.editor.side;
        let (my_edge, their_edge) = side.edges();
        Some(config::LinkConfig {
            id: format!("gui-{}-{}", short_key(this), short_key(other)),
            from: config::EndpointConfig {
                node: node_reference(this),
                display: config::AUTO_DISPLAY.to_owned(),
                edge: my_edge,
                start: 0.0,
                end: 0.0,
            },
            to: config::EndpointConfig {
                node: node_reference(other),
                display: config::AUTO_DISPLAY.to_owned(),
                edge: their_edge,
                start: 0.0,
                end: 0.0,
            },
            bidirectional: true,
        })
    }

    // ------------------------------------------------------------ local actions

    /// Applies everything in an [`Action`] that never reaches the core.
    ///
    /// The wire requests are the app's business (it hands them to [`GuiClient`]); this is the
    /// half that changes what the window shows: which screen is open, what the user is
    /// typing, where they dragged a rectangle.
    pub fn local(&mut self, action: &Action) {
        match action {
            Action::ShowScreen(screen) => self.screen = *screen,
            Action::DismissOutcome => self.outcome = None,
            Action::SetAddress(address) => self.pairing.address = address.clone(),
            Action::ToggleDetails(open) => self.pairing.details = *open,
            Action::ClosePairingWindow => {
                self.pairing.window = None;
                self.pairing.prompt = None;
                self.push_log("Окно сопряжения закрыто в окне (служба закроет его по таймауту)");
            }
            Action::SetSide(side) => {
                self.editor.side = *side;
                self.place_other(*side);
            }
            Action::MoveDisplay {
                node_id,
                display_id,
                dx,
                dy,
            } => self.move_display(node_id, display_id, *dx, *dy),
            Action::ChooseSelf(node_id) => self.editor.self_choice = node_id.clone(),
            Action::ChooseOther(node_id) => self.editor.other_choice = node_id.clone(),
            Action::SelectRevoke(node_id) => {
                self.settings.revoke = node_id.clone();
                self.settings.revoke_confirmed = false;
            }
            Action::ConfirmRevoke(confirmed) => self.settings.revoke_confirmed = *confirmed,
            Action::SelectLanAddress(address) => self.settings.lan.choice = Some(address.clone()),
            Action::ApplyLanAddress => self.apply_lan_address(),
            // «Перезапустить службу» is not a fold: it decides against the pid the window
            // holds and asks for a process to be stopped, so `Bridge::dispatch` drives it.
            Action::RestartCore => {}
            Action::DismissFirstRun => self.settings.first_run = false,
            Action::Refresh => self.conflict = None,
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
        let current = self.config.listen_address.clone();
        let chosen = self.settings.lan.selected(&current).unwrap_or_default();
        let Some(address) = chosen.parse::<std::net::Ipv4Addr>().ok() else {
            self.settings.lan.applied = Some(Outcome::failed(
                "адрес не выбран — выберите адрес в списке и нажмите «Применить» снова",
            ));
            return;
        };
        let Some(paths) = self.config.paths.clone() else {
            self.settings.lan.applied = Some(Outcome::failed(
                "каталоги настроек не определены — перезапустите окно без особых параметров",
            ));
            return;
        };
        match config::first_run::set_listen_address(&paths, address) {
            Ok(()) => {
                self.push_log(format!("Адрес этого компьютера: {address}"));
                self.config.listen_address = address.to_string();
                self.settings.lan.choice = None;
                self.settings.lan.applied = Some(Outcome::done("Адрес сохранён"));
            }
            Err(why) => {
                self.settings.lan.applied = Some(Outcome::failed(format!(
                    "адрес не сохранён ({})",
                    short_error(&why)
                )));
            }
        }
    }

    /// «Перезапустить службу»: stop the core this window started, then start a fresh one.
    ///
    /// The pid comes from [`ViewModel::core`] and from nowhere else. A service this window
    /// did not start — one opened by hand, or by another window — is not the window's to
    /// stop, so it says so instead of killing a process it only *thinks* it knows.
    ///
    /// What to do with the answer is the caller's: stopping a process is the one thing a
    /// view model cannot be handed back.
    pub fn begin_restart(&mut self, now: Instant) -> RestartStep {
        if self.settings.service.restart.is_some() {
            return RestartStep::Refused;
        }
        let Some(core) = &self.core else {
            self.settings.service.line = Some(Outcome::failed(
                "служба запущена не этим окном — остановите её сами",
            ));
            return RestartStep::Refused;
        };
        let Some(pid) = core.pid else {
            self.settings.service.line = Some(Outcome::failed(
                "номер процесса службы неизвестен — остановите её сами",
            ));
            return RestartStep::Refused;
        };
        if !self.connection.is_connected() {
            // No link to break, so nothing to wait for: the fresh core can be started now.
            self.push_log(format!("Служба перезапускается (pid {pid})"));
            self.settings.service.restart = Some(Restart::Starting);
            return RestartStep::Start;
        }
        self.push_log(format!("Служба останавливается (pid {pid})"));
        self.settings.service.restart = Some(Restart::Stopping { since: now });
        RestartStep::Stop(pid)
    }

    /// What the stop of the core process answered, when it answered at all.
    ///
    /// A stop that failed ends the restart: the window says why, and leaves the service
    /// alone. A stop that was sent leaves the [`Restart::Stopping`] wait standing, because
    /// only a dropped link proves the core is gone.
    pub fn on_core_stopped(&mut self, stopped: Result<(), String>) {
        if let Err(why) = stopped {
            self.settings.service.restart = None;
            self.push_log(format!("Не удалось остановить службу: {why}"));
            self.settings.service.line = Some(Outcome::failed(format!(
                "не удалось остановить службу ({why})"
            )));
        }
    }

    /// Puts the other computer's displays on the side the user picked.
    ///
    /// Choosing a side is a placement, not a permission: it moves the other node's
    /// rectangles next to this node's, and the user may then drag them. Both write the same
    /// `logical_*` the document carries.
    fn place_other(&mut self, side: Side) {
        let (Some(this), Some(other)) = (
            self.self_node().map(str::to_owned),
            self.other_node().map(str::to_owned),
        ) else {
            return;
        };
        let Some(my_bounds) = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == this)
            .and_then(EditorNode::bounds)
        else {
            return;
        };
        let Some(their_bounds) = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == other)
            .and_then(EditorNode::bounds)
        else {
            return;
        };
        let (dx, dy) = match side {
            Side::Left => (my_bounds.0 - (their_bounds.2 - their_bounds.0), my_bounds.1),
            Side::Right => (my_bounds.2, my_bounds.1),
            Side::Top => (my_bounds.0, my_bounds.1 - (their_bounds.3 - their_bounds.1)),
            Side::Bottom => (my_bounds.0, my_bounds.3),
        };
        let Some(node) = self
            .editor
            .nodes
            .iter_mut()
            .find(|node| node.node_id == other)
        else {
            return;
        };
        for display in &mut node.displays {
            display.shift(dx - their_bounds.0, dy - their_bounds.1);
        }
    }

    /// Moves one display by a delta in its own node's logical units.
    ///
    /// The peer's block has one position — next to the edge the radio names — so a drag is a
    /// gesture that *chooses* that edge: the display follows the pointer, the nearest side to
    /// where it ended up becomes the side, and the block snaps onto it. Nothing the user can
    /// drag can leave the two computers overlapping, which is what makes the preview and
    /// «Применить» the same picture.
    fn move_display(&mut self, node_id: &str, display_id: &str, dx: f64, dy: f64) {
        let Some(moved) = self
            .editor
            .nodes
            .iter_mut()
            .find(|node| node.node_id == node_id)
            .and_then(|node| {
                node.displays
                    .iter_mut()
                    .find(|display| display.id == display_id)
            })
        else {
            return;
        };
        moved.shift(dx, dy);
        let Some(other_bounds) = self
            .editor
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .and_then(EditorNode::bounds)
        else {
            return;
        };
        let Some(mine) = self
            .self_node()
            .and_then(|this| self.editor.nodes.iter().find(|node| node.node_id == this))
            .and_then(EditorNode::bounds)
        else {
            return;
        };
        let side = nearest_side(mine, other_bounds);
        if side != self.editor.side {
            self.editor.side = side;
        }
        self.place_other(side);
    }
}

// ---------------------------------------------------------------- actions

/// Everything the window can ask for.
///
/// The first group travels to the core (as the `ipc.proto` request it names); the last group
/// is applied to the view model by the app. [`Action::wire`] is the boundary between them.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
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
    /// Answer the six-digit confirmation.
    Answer {
        /// The attempt the prompt named.
        attempt_id: Vec<u8>,
        /// `true` for «Да, совпадают».
        approve: bool,
    },
    /// Turn the shared clipboard on or off.
    SetClipboard(bool),
    /// Rebuild the shared layout from this desktop (`LayoutAuto`).
    LayoutAuto,
    /// Apply the edited layout document.
    UpdateTopology {
        /// The document, in the `[topology]` schema.
        layout_toml: Vec<u8>,
        /// The revision the document was edited against.
        expected_revision: u64,
        /// The author of that revision; empty for the first document.
        expected_author: Vec<u8>,
    },
    /// Ask for the divergent edits kept after a partition.
    ListPendingEdits,
    /// Apply one of them.
    ApplyPendingEdit(Vec<u8>),
    /// Drop one of them.
    DiscardPendingEdit(Vec<u8>),
    /// Forget the portal permissions of this node.
    ResetPortalGrants,
    /// Ask the core to retry its degraded input agent after the user presses «Повторить».
    RetryAgent,
    /// Revoke a paired computer.
    RevokePeer(Vec<u8>),
    /// Ask the core to write its diagnostics.
    ExportDiagnostics(String),
    /// Start `lws-core run` for this user.
    StartCore,
    /// Try the core again right now instead of waiting for the 2 s retry.
    ConnectAgain,
    /// Show a screen.
    ShowScreen(Screen),
    /// Forget the last finished operation.
    DismissOutcome,
    /// The text of «Подключиться по адресу».
    SetAddress(String),
    /// Open or close the fingerprints under the code (ADR-010).
    ToggleDetails(bool),
    /// Drop the local pairing state; the core closes its own window by timeout.
    ClosePairingWindow,
    /// Which side of this computer the other one is on.
    SetSide(Side),
    /// Drag a display by `dx`/`dy` logical units of its own node.
    MoveDisplay {
        /// The node the display belongs to.
        node_id: String,
        /// The display's id.
        display_id: String,
        /// Horizontal drag in logical units.
        dx: f64,
        /// Vertical drag in logical units.
        dy: f64,
    },
    /// Name the member that is this computer.
    ChooseSelf(Option<String>),
    /// Name the member the layout links to.
    ChooseOther(Option<String>),
    /// Choose the member to revoke.
    SelectRevoke(Option<String>),
    /// Answer the revoke confirmation.
    ConfirmRevoke(bool),
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
}

impl Action {
    /// The request this action puts on the wire, or `None` for a local one.
    pub fn wire(&self) -> Option<(Request, Msg)> {
        let wire = match self {
            Action::Refresh => (Request::GetStatus, Msg::GetStatus(ipc::GetStatus {})),
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
            Action::LayoutAuto => (Request::LayoutAuto, Msg::LayoutAuto(ipc::LayoutAuto {})),
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
            Action::ListPendingEdits => (
                Request::ListPendingEdits,
                Msg::ListPendingEdits(ipc::ListPendingEdits {}),
            ),
            Action::ApplyPendingEdit(edit_id) => (
                Request::ApplyPendingEdit,
                Msg::ApplyPendingEdit(ipc::ApplyPendingEdit {
                    edit_id: edit_id.clone(),
                }),
            ),
            Action::DiscardPendingEdit(edit_id) => (
                Request::DiscardPendingEdit,
                Msg::DiscardPendingEdit(ipc::DiscardPendingEdit {
                    edit_id: edit_id.clone(),
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
    /// «Обновить» on the conflict line is a `GetTopology`; [`Action::Refresh`] asks for both
    /// the status and the document, so it has two.
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
}

impl Bridge {
    /// Wires a view model to the client that serves it.
    pub fn new(view: ViewModel, client: GuiClient) -> Self {
        Bridge { view, client }
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

    /// Sends one action where it belongs.
    ///
    /// Fire-and-forget, as `docs/03-architecture.md` («GUI ↔ core») requires: the request is
    /// recorded against `now` and handed to the client thread, and the answer arrives later as
    /// a frame [`Bridge::pump`] folds.
    pub fn dispatch(&mut self, action: Action, now: Instant) {
        // The local half first: a refresh clears the conflict line, and none of the local
        // actions care whether the wire half below succeeds.
        self.view.local(&action);
        if matches!(&action, Action::StopCore) && !self.view.begin_stop(now) {
            return;
        }
        if let Some((request, message)) = action.wire() {
            let id = self.view.begin_request(request, now);
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
                self.client.start_core();
            }
            Action::ConnectAgain => {
                self.client.connect_again();
            }
            Action::RestartCore => self.restart_core(now),
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

    /// «Перезапустить службу»: stop the core this window started, then ask for a new one.
    ///
    /// Only the pid from [`ViewModel::core`] is eligible for process termination; «Остановить
    /// службу» first asks the core to exit and uses that pid only after its timeout.
    /// The wait for the old core to be gone is event-driven — [`Bridge::pump`] starts the
    /// fresh one when the link drops.
    fn restart_core(&mut self, now: Instant) {
        match self.view.begin_restart(now) {
            RestartStep::Stop(pid) => self.view.on_core_stopped(stop_core(pid)),
            RestartStep::Start => {
                self.client.start_core();
            }
            RestartStep::Refused => {}
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
                GuiEvent::Connected { endpoint } => {
                    self.view.on_connected(&endpoint);
                    // §4 of the build spec: a fresh link asks for the status and the document.
                    self.dispatch(Action::Refresh, Instant::now());
                }
                GuiEvent::Disconnected { why } => {
                    // A dropped link is how the window knows the core it stopped is gone.
                    if self.view.on_disconnected(why) {
                        self.client.start_core();
                    }
                }
                GuiEvent::Frame(frame) => self.view.apply(*frame),
                GuiEvent::NotSent { id } => self.view.on_not_sent(id),
                GuiEvent::CoreStarted { started } => self.view.on_core_started(started),
            }
        }
        if let Some(pid) = self.view.take_timed_out_shutdown_pid(Instant::now()) {
            self.view.on_shutdown_fallback(stop_core(pid));
        }
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
    StartCore,
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
}

impl GuiClient {
    /// Starts the client thread for `endpoint`.
    ///
    /// `endpoint` is `None` when the endpoint could not be resolved: the thread then only
    /// answers «не отправлено», which is exactly what the window says on that host.
    pub fn start(endpoint: Option<Endpoint>, launch: Launch) -> Self {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
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
                            why: format!("не удалось запустить обмен со службой: {error}"),
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
        }
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
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.send(Command::StartCore).is_ok())
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
    events: tokio::sync::mpsc::UnboundedSender<GuiEvent>,
) {
    let Some(endpoint) = endpoint else {
        // Nothing to connect to: the host has no resolvable endpoint at all. The window
        // still has to answer its buttons.
        while let Some(command) = commands.recv().await {
            answer_offline(command, &launch, &events);
        }
        return;
    };
    loop {
        match ::ipc::transport::connect(&endpoint).await {
            Ok(stream) => {
                let _ = events.send(GuiEvent::Connected {
                    endpoint: endpoint.describe(),
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
                let _ = events.send(GuiEvent::Disconnected {
                    why: error.to_string(),
                });
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
                    Some(Command::StartCore) => {
                        let started = launch_core(&launch);
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
    events: &tokio::sync::mpsc::UnboundedSender<GuiEvent>,
) -> Option<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
            Step::Command(Some(Command::StartCore)) => {
                // A core is already answering here, so «Запустить» has nothing to do; saying
                // so beats starting a second daemon.
                let _ = events.send(GuiEvent::CoreStarted {
                    started: Err("служба уже запущена и отвечает".to_owned()),
                });
            }
        }
    }
}

/// Answers a command while no link exists.
fn answer_offline(
    command: Command,
    launch: &Launch,
    events: &tokio::sync::mpsc::UnboundedSender<GuiEvent>,
) {
    match command {
        Command::Send { id, .. } => {
            let _ = events.send(GuiEvent::NotSent { id });
        }
        Command::StartCore => {
            let started = launch_core(launch);
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
    /// The global flags, exactly as `lws-core` wants them.
    pub flags: Vec<String>,
}

impl Launch {
    /// A launch that names the documented directories.
    pub fn with_flags(flags: Vec<String>) -> Self {
        Launch { flags }
    }
}

/// The global options of the command line, mirroring `lws-core`'s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliArgs {
    /// `--root DIR`.
    pub root: Option<PathBuf>,
    /// `--config-dir DIR`.
    pub config_dir: Option<PathBuf>,
    /// `--data-dir DIR`.
    pub data_dir: Option<PathBuf>,
    /// `--state-dir DIR`.
    pub state_dir: Option<PathBuf>,
    /// `--runtime-dir DIR`.
    pub runtime_dir: Option<PathBuf>,
}

impl CliArgs {
    /// Parses the global flags, ignoring what it does not know (a window that refuses to open
    /// because of a stray argument helps nobody).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Self {
        let mut parsed = CliArgs::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut take = |slot: &mut Option<PathBuf>| {
                if let Some(value) = args.next() {
                    *slot = Some(PathBuf::from(value));
                }
            };
            match arg.as_str() {
                "--root" => take(&mut parsed.root),
                "--config-dir" => take(&mut parsed.config_dir),
                "--data-dir" => take(&mut parsed.data_dir),
                "--state-dir" => take(&mut parsed.state_dir),
                "--runtime-dir" => take(&mut parsed.runtime_dir),
                _ => {}
            }
        }
        parsed
    }

    /// The same options as the flags `lws-core` takes, in the order its usage line lists.
    pub fn flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for (name, value) in [
            ("--root", &self.root),
            ("--config-dir", &self.config_dir),
            ("--data-dir", &self.data_dir),
            ("--state-dir", &self.state_dir),
            ("--runtime-dir", &self.runtime_dir),
        ] {
            if let Some(value) = value {
                flags.push(name.to_owned());
                flags.push(value.display().to_string());
            }
        }
        flags
    }

    /// Resolves the directories: the documented defaults, with every flag on top.
    ///
    /// The same rule as `apps/lws-core/src/main.rs::resolve_paths`, so the window and the
    /// core it starts agree on `config/`, `data/`, `state/` and `run/`.
    pub fn resolve(&self) -> Result<Paths, String> {
        let defaults = config::default_paths().map_err(|error| error.to_string())?;
        let under_root = |name: &str| self.root.as_ref().map(|root| root.join(name));
        Ok(Paths::new(
            self.config_dir
                .clone()
                .or_else(|| under_root("config"))
                .unwrap_or_else(|| defaults.config_dir().to_path_buf()),
            self.data_dir
                .clone()
                .or_else(|| under_root("data"))
                .unwrap_or_else(|| defaults.data_dir().to_path_buf()),
            self.state_dir
                .clone()
                .or_else(|| under_root("state"))
                .unwrap_or_else(|| defaults.state_dir().to_path_buf()),
            self.runtime_dir
                .clone()
                .or_else(|| under_root("run"))
                .or_else(|| defaults.runtime_dir().map(std::path::Path::to_path_buf)),
        ))
    }
}

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

/// The program name `lws-core` runs under on this host.
fn core_program_name() -> &'static str {
    if cfg!(windows) {
        "lws-core.exe"
    } else {
        "lws-core"
    }
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
        Ok(status) => Err(format!(
            "{program} не остановил процесс {pid} (код {})",
            status
                .code()
                .map_or("неизвестен".to_owned(), |code| code.to_string())
        )),
        Err(error) => Err(format!("{program} не запустился: {error}")),
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

/// Starts `lws-core run` detached, with the flags the window was given.
///
/// `docs/03-architecture.md`: «Закрытие окна GUI не влияет на активный сеанс». The child gets
/// no inherited standard streams, so its lifetime is its own; it is never waited for.
pub fn launch_core(launch: &Launch) -> Result<StartedCore, String> {
    let name = core_program_name();
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|candidate| candidate.is_file());
    let program = beside_exe.unwrap_or_else(|| PathBuf::from(name));
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
        Ok(child) => Ok(StartedCore {
            program,
            pid: Some(child.id()),
        }),
        Err(error) => Err(format!("{}: {error}", program.display())),
    }
}

/// Opens the window's two halves: the state that draws and the client that talks.
pub fn boot(args: CliArgs) -> (ViewModel, GuiClient) {
    let launch = Launch::with_flags(args.flags());
    let paths = match args.resolve() {
        Ok(paths) => paths,
        Err(why) => {
            let mut view = ViewModel::new(
                CoreEndpoint::Failed {
                    why: format!("не удалось определить каталоги: {why}"),
                },
                LocalConfig::default(),
            );
            view.push_log(format!(
                "Не удалось определить каталоги: {}",
                short_error(&why)
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

/// The length of the edge two displays share, in logical units.
fn edge_span(mine: f64, theirs: f64) -> f64 {
    let span = mine.min(theirs);
    if span.is_finite() && span > 0.0 {
        span
    } else {
        0.0
    }
}

/// Which side of `mine` the block `theirs` was dragged to.
///
/// The centres are compared, so a block dragged past a corner snaps to the edge it is
/// *nearest* to and not to whichever direction an `if`-chain tests first — which is what makes
/// a diagonal drag feel predictable.
fn nearest_side(mine: (f64, f64, f64, f64), theirs: (f64, f64, f64, f64)) -> Side {
    let (dx, dy) = (
        (theirs.0 + theirs.2) - (mine.0 + mine.2),
        (theirs.1 + theirs.3) - (mine.1 + mine.3),
    );
    if dx.abs() >= dy.abs() {
        if dx < 0.0 {
            Side::Left
        } else {
            Side::Right
        }
    } else if dy < 0.0 {
        Side::Top
    } else {
        Side::Bottom
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
fn author_label(author: &str) -> String {
    if author.is_empty() {
        "неизвестно".to_owned()
    } else {
        format!("компьютер {}", short_id(author))
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
        Msg::PairingAnswer(_) => "PairingAnswer",
        Msg::UpdateTopology(_) => "UpdateTopology",
        Msg::SetClipboard(_) => "SetClipboard",
        Msg::RevokePeer(_) => "RevokePeer",
        Msg::ExportDiagnostics(_) => "ExportDiagnostics",
        Msg::ResetPortalGrants(_) => "ResetPortalGrants",
        Msg::RetryAgent(_) => "RetryAgent",
        Msg::LayoutAuto(_) => "LayoutAuto",
        Msg::Shutdown(_) => "Shutdown",
        Msg::ClaimOwnership(_) => "ClaimOwnership",
        Msg::AbandonOwnership(_) => "AbandonOwnership",
        Msg::ListPendingEdits(_) => "ListPendingEdits",
        Msg::ApplyPendingEdit(_) => "ApplyPendingEdit",
        Msg::DiscardPendingEdit(_) => "DiscardPendingEdit",
        Msg::Status(_) => "Status",
        Msg::PairingPrompt(_) => "PairingPrompt",
        Msg::OpResult(_) => "OpResult",
        Msg::TopologyDoc(_) => "TopologyDoc",
        Msg::PendingEdits(_) => "PendingEdits",
        Msg::PairingWindow(_) => "PairingWindow",
    }
}

/// What a code means, in a sentence the owner can act on.
fn code_label(code: &str) -> String {
    match code {
        "" => "без кода".to_owned(),
        "EMERGENCY_HOTKEY" => "нет аварийной клавиши".to_owned(),
        "RECEIVER_TAKEOVER" => "нет завершения сеанса по физическому вводу".to_owned(),
        "TEXT_CLIPBOARD" => "общий буфер обмена недоступен".to_owned(),
        "ABSOLUTE_POINTER" => "нет абсолютного курсора".to_owned(),
        "CAPTURE" => "нет перехвата ввода".to_owned(),
        "INJECT" => "нет ввода событий".to_owned(),
        "LAYOUT_CONFLICT" => "раскладку изменили на другом компьютере".to_owned(),
        "INVALID_TOPOLOGY" => "документ раскладки неверный".to_owned(),
        "NO_COORDINATOR" => "нет координатора (владельца)".to_owned(),
        "PENDING_VERDICT" => "координатор не подтвердил правку".to_owned(),
        "UNKNOWN_EDIT" => "такой правки больше нет".to_owned(),
        "UNKNOWN_PEER" => "этот компьютер не в паре".to_owned(),
        "TRUST_WRITE" => "не удалось записать список пар".to_owned(),
        "CLIPBOARD_DISABLED" => "буфер обмена выключен в настройках".to_owned(),
        "EXPORT_FAILED" => "не удалось записать файл диагностики".to_owned(),
        "PAIRING_FAILED" => "не удалось договориться о сопряжении".to_owned(),
        "PAIRING_GONE" => "окно сопряжения уже закрылось".to_owned(),
        "PAIRING_WINDOW_OPEN" => "окно сопряжения уже открыто".to_owned(),
        "NO_WINDOW" => "окно сопряжения не открыто".to_owned(),
        "BAD_ADDRESS" => "адрес неверный".to_owned(),
        "UNKNOWN_ATTEMPT" => "это сопряжение уже не ждёт ответа".to_owned(),
        "CLAIM_REFUSED" => "взять управление не получилось".to_owned(),
        "SWITCH_REFUSED" => "переключение не получилось".to_owned(),
        "PORTAL_STORE" => "порталы не ответили".to_owned(),
        "AGENT_LOST" => "агент ввода потерян".to_owned(),
        "BAD_REQUEST" => "ядро не поняло запрос".to_owned(),
        other => format!("код {other}"),
    }
}

/// The sentence a successful request is reported with.
fn success_text(request: Request) -> String {
    match request {
        Request::Claim => "управление у этого компьютера".to_owned(),
        Request::Abandon => "управление отдано".to_owned(),
        Request::PairWith => "подключение начато".to_owned(),
        Request::PairingAnswer => "ответ отправлен".to_owned(),
        Request::UpdateTopology => "раскладка отправлена".to_owned(),
        // The window's own wording is the `ViewModel::success_text` one (it can name the
        // computer and the side); this is the plain fall-back.
        Request::LayoutAuto => "расположено автоматически".to_owned(),
        Request::SetClipboard => "буфер обмена переключён".to_owned(),
        Request::RevokePeer => "компьютер удалён из пары".to_owned(),
        Request::ResetPortalGrants => "разрешения порталов забыты".to_owned(),
        Request::RetryAgent => "повторный запуск службы ввода начат".to_owned(),
        Request::Shutdown => "остановка службы начата".to_owned(),
        Request::ExportDiagnostics => "диагностика сохранена".to_owned(),
        Request::StartPairing => "окно сопряжения открыто".to_owned(),
        Request::ApplyPendingEdit => "правка отправлена координатору".to_owned(),
        Request::DiscardPendingEdit => "правка отброшена".to_owned(),
        Request::GetStatus | Request::GetTopology => "ядро ответило".to_owned(),
        Request::ListPendingEdits => "список правок получен".to_owned(),
    }
}

/// The sentence a failed request is reported with.
fn failure_text(request: Request, code: &str) -> String {
    let label = code_label(code);
    match request {
        Request::LayoutAuto => {
            format!("автоматическая раскладка не готова: {label}")
        }
        Request::UpdateTopology if code == "LAYOUT_CONFLICT" => {
            "раскладку изменили на другом компьютере — обновите документ и повторите".to_owned()
        }
        Request::PairingAnswer if code == "PAIRING_FAILED" => {
            "коды не совпали или партнёр отказался".to_owned()
        }
        _ => label,
    }
}

// ---------------------------------------------------------------- boot helpers

/// The GUI's own arguments, read from the process command line.
pub fn cli_args_from_env() -> CliArgs {
    CliArgs::parse(std::env::args().skip(1))
}
