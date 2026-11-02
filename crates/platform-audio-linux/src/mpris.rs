//! MPRIS has no global current-player API. Prefer the most recently observed transition
//! to Playing; initial discovery ties use sorted bus names. With none Playing, retain only
//! the last selected still-live Paused/Stopped player so remote Play remains possible.
mod model;
use crate::backend::lock;
use audio_backend::*;
use futures::{
    future::{select, BoxFuture, Either},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use model::{Player, Properties};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zbus::{zvariant::OwnedValue, Connection, MatchRule, Message, MessageStream, Proxy};
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const MAX_PLAYERS: usize = 64;
const COMMAND_CAPACITY: usize = 16;
const RPC_TIMEOUT: Duration = Duration::from_millis(250);
type CommandFuture = BoxFuture<'static, (u64, Result<(), BackendError>)>;
struct Pending {
    ticket: u64,
    request: MediaCommandRequest,
    deadline: Instant,
    dispatched: bool,
}
struct Data {
    available: bool,
    players: BTreeMap<String, Player>,
    next_token: u64,
    next_ticket: u64,
    order: u64,
    selected: Option<MediaSessionToken>,
    pending: VecDeque<Pending>,
    completed: VecDeque<Event>,
    changed: VecDeque<Event>,
    overflow: bool,
}
struct Shared {
    data: Mutex<Data>,
    stop: AtomicBool,
}
impl Data {
    fn new() -> Self {
        Self {
            available: false,
            players: BTreeMap::new(),
            next_token: 0,
            next_ticket: 0,
            order: 0,
            selected: None,
            pending: VecDeque::with_capacity(COMMAND_CAPACITY),
            completed: VecDeque::with_capacity(COMMAND_CAPACITY),
            changed: VecDeque::with_capacity(EVENT_CAPACITY),
            overflow: false,
        }
    }
    fn emit(&mut self, event: Event) {
        if let Event::MediaChanged { session, .. } = &event {
            if let Some(slot) = self.changed.iter_mut().find(
                |queued| matches!(queued,Event::MediaChanged {session:other,..} if other==session),
            ) {
                *slot = event;
                return;
            }
        }
        if self.changed.len() == EVENT_CAPACITY {
            self.changed.pop_front();
            self.overflow = true;
        }
        self.changed.push_back(event);
    }
    fn complete(&mut self, id: u64, result: Result<(), BackendError>) {
        if let Some(index) = self.pending.iter().position(|pending| pending.ticket == id) {
            let pending = self.pending.remove(index).expect("found above");
            self.completed.push_back(Event::MediaCommandCompleted {
                request_id: pending.request.request_id,
                result,
            });
        }
    }
    fn expire(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.pending.len() {
            if now >= self.pending[index].deadline {
                let id = self.pending[index].ticket;
                self.complete(id, Err(BackendError::Timeout));
            } else {
                index += 1;
            }
        }
    }
    fn cancel(&mut self, error: BackendError) {
        while let Some(pending) = self.pending.pop_front() {
            self.completed.push_back(Event::MediaCommandCompleted {
                request_id: pending.request.request_id,
                result: Err(error),
            });
        }
    }
    fn remove(&mut self, name: &str) {
        if let Some(player) = self.players.remove(name) {
            if self.selected == Some(player.token) {
                self.selected = None;
            }
            let mut index = 0;
            while index < self.pending.len() {
                if self.pending[index].request.session == player.token {
                    let id = self.pending[index].ticket;
                    self.complete(id, Err(BackendError::DeviceGone));
                } else {
                    index += 1;
                }
            }
            self.emit(Event::MediaChanged {
                session: player.token,
                info: None,
            });
        }
    }
    fn publish(&mut self, name: &str, changed: bool, was_playing: bool, now: Instant) {
        let Some(player) = self.players.get_mut(name) else {
            return;
        };
        if self.selected == Some(player.token) && player.state == PlaybackState::Unknown {
            self.selected = None;
        }
        if player.state == PlaybackState::Playing && !was_playing {
            self.order = self.order.saturating_add(1);
            player.playing_order = self.order;
        }
        if changed {
            if !player.advance_revision() {
                self.remove(name);
                return;
            }
            let event = Event::MediaChanged {
                session: player.token,
                info: Some(player.info(now)),
            };
            self.emit(event);
        }
    }
    fn unavailable(&mut self) {
        self.available = false;
        self.selected = None;
        self.cancel(BackendError::ServerUnavailable);
        while let Some((_, player)) = self.players.pop_first() {
            self.emit(Event::MediaChanged {
                session: player.token,
                info: None,
            });
        }
    }
}
struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        lock(&self.0.data).unavailable();
    }
}
/// Native work never runs on a Pulse callback/reactor thread. Command completion storage is
/// reserved at admission until polled; metadata events coalesce independently.
pub(crate) struct Mpris {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl Mpris {
    pub fn start() -> Self {
        let shared = Arc::new(Shared {
            data: Mutex::new(Data::new()),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("bfa-mpris".into())
            .spawn(move || {
                let _exit = WorkerExit(worker.clone());
                let _ = futures::executor::block_on(run(worker, ready_tx));
            })
            .ok();
        if thread.is_some() {
            let _ = ready_rx.recv_timeout(Duration::from_secs(2));
        }
        Self { shared, thread }
    }
    pub fn availability(&self) -> Availability {
        if lock(&self.shared.data).available {
            Availability::Available
        } else {
            Availability::Unavailable(BackendError::ServerUnavailable)
        }
    }
    pub fn sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        let data = lock(&self.shared.data);
        if !data.available {
            return Err(BackendError::ServerUnavailable);
        }
        let now = Instant::now();
        Ok(data
            .players
            .values()
            .map(|player| player.info(now))
            .collect())
    }
    pub fn current(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        let mut data = lock(&self.shared.data);
        if !data.available {
            return Err(BackendError::ServerUnavailable);
        }
        let selected = data
            .players
            .values()
            .filter(|player| player.token.0 != 0 && player.state == PlaybackState::Playing)
            .max_by_key(|player| player.playing_order)
            .map(|player| player.token)
            .or_else(|| {
                data.players
                    .values()
                    .find(|player| {
                        Some(player.token) == data.selected
                            && player.token.0 != 0
                            && matches!(
                                player.state,
                                PlaybackState::Paused | PlaybackState::Stopped
                            )
                    })
                    .map(|player| player.token)
            });
        data.selected = selected;
        Ok(selected)
    }
    pub fn command(&self, request: MediaCommandRequest) -> Result<(), BackendError> {
        let mut data = lock(&self.shared.data);
        let now = Instant::now();
        data.expire(now);
        if !data.available {
            return Err(BackendError::ServerUnavailable);
        }
        if request.request_id == 0 {
            return Err(BackendError::InvalidArgument);
        }
        if data.pending.len()+data.completed.len()>=COMMAND_CAPACITY||data.pending.iter().any(|pending|pending.request.request_id==request.request_id)||data.completed.iter().any(|event|matches!(event,Event::MediaCommandCompleted {request_id,..} if *request_id==request.request_id)){return Err(BackendError::Busy);}
        let player = data
            .players
            .values()
            .find(|player| player.token == request.session)
            .ok_or(BackendError::DeviceGone)?;
        if player.revision != request.expected_revision {
            return Err(BackendError::StaleRevision);
        }
        if !player.commands().contains(request.command) {
            return Err(BackendError::Unsupported);
        }
        if let MediaCommand::SeekTo { position_ms } = request.command {
            player.seek_position(position_ms)?;
        }
        let ticket = data.next_ticket.checked_add(1).ok_or(BackendError::Busy)?;
        data.next_ticket = ticket;
        data.pending.push_back(Pending {
            ticket,
            request,
            deadline: now + Duration::from_millis(u64::from(MEDIA_COMMAND_TIMEOUT_MS)),
            dispatched: false,
        });
        Ok(())
    }
    pub fn cancel(&self) {
        lock(&self.shared.data).cancel(BackendError::Cancelled);
    }
    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
    pub fn poll_completion(&self) -> Option<Event> {
        let mut data = lock(&self.shared.data);
        data.expire(Instant::now());
        data.completed.pop_front()
    }
    pub fn poll_changed(&self) -> Option<Event> {
        let mut data = lock(&self.shared.data);
        if data.overflow {
            data.overflow = false;
            Some(Event::ResyncRequired)
        } else {
            data.changed.pop_front()
        }
    }
}
impl Drop for Mpris {
    fn drop(&mut self) {
        self.shutdown();
    }
}
async fn bounded<T>(
    duration: Duration,
    future: impl Future<Output = zbus::Result<T>>,
) -> Result<T, BackendError> {
    futures::pin_mut!(future);
    let timeout = futures_timer::Delay::new(duration);
    futures::pin_mut!(timeout);
    match select(future, timeout).await {
        Either::Left((result, _)) => result.map_err(|_| BackendError::ServerUnavailable),
        Either::Right(_) => Err(BackendError::Timeout),
    }
}
async fn bus_proxy(connection: &Connection) -> zbus::Result<Proxy<'_>> {
    Proxy::new(connection, BUS, BUS_PATH, BUS).await
}
async fn owner(connection: &Connection, name: &str) -> Result<String, BackendError> {
    bounded(RPC_TIMEOUT, async {
        bus_proxy(connection)
            .await?
            .call("GetNameOwner", &(name,))
            .await
    })
    .await
}
async fn all(
    connection: &Connection,
    owner: &str,
    interface: &str,
) -> Result<Properties, BackendError> {
    bounded(RPC_TIMEOUT, async {
        Proxy::new(connection, owner, PATH, PROPERTIES)
            .await?
            .call("GetAll", &(interface,))
            .await
    })
    .await
}
async fn stream(
    connection: &Connection,
    interface: &str,
    member: &str,
    path: &str,
    bus_sender: bool,
) -> Result<MessageStream, BackendError> {
    let mut rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)
        .map_err(|_| BackendError::InvalidArgument)?
        .member(member)
        .map_err(|_| BackendError::InvalidArgument)?
        .path(path)
        .map_err(|_| BackendError::InvalidArgument)?;
    if bus_sender {
        rule = rule
            .sender(BUS)
            .map_err(|_| BackendError::InvalidArgument)?;
    }
    bounded(
        RPC_TIMEOUT,
        MessageStream::for_match_rule(rule.build(), connection, Some(64)),
    )
    .await
}
async fn discover(
    connection: &Connection,
    shared: &Shared,
    name: String,
    expected_owner: Option<String>,
) {
    if !name.starts_with(PREFIX) || shared.stop.load(Ordering::Acquire) {
        return;
    }
    {
        let mut data = lock(&shared.data);
        if data
            .players
            .get(&name)
            .is_some_and(|player| expected_owner.as_ref() == Some(&player.owner))
        {
            return;
        }
        data.remove(&name);
        if data.players.len() >= MAX_PLAYERS {
            data.overflow = true;
            return;
        }
    }
    let Ok(owner) = owner(connection, &name).await else {
        return;
    };
    if expected_owner
        .as_ref()
        .is_some_and(|expected| expected != &owner)
    {
        return;
    }
    let Ok(root) = all(connection, &owner, ROOT).await else {
        return;
    };
    let Ok(properties) = all(connection, &owner, PLAYER).await else {
        return;
    };
    let now = Instant::now();
    let mut data = lock(&shared.data);
    let Some(next) = data.next_token.checked_add(1) else {
        data.overflow = true;
        return;
    };
    data.next_token = next;
    let mut player = Player::new(owner, MediaSessionToken(next), now);
    player.apply(ROOT, root, now);
    player.apply(PLAYER, properties, now);
    player.next_poll = now + Duration::from_secs(1);
    data.players.insert(name.clone(), player);
    data.publish(&name, true, false, now);
}
async fn changed(connection: &Connection, shared: &Shared, message: Message) {
    let Some(sender) = message
        .header()
        .sender()
        .map(|sender| sender.as_str().to_owned())
    else {
        return;
    };
    let Ok((interface, properties, invalidated)) = message
        .body()
        .deserialize::<(String, Properties, Vec<String>)>()
    else {
        return;
    };
    if interface != PLAYER && interface != ROOT {
        return;
    }
    let names: Vec<_> = lock(&shared.data)
        .players
        .iter()
        .filter(|(_, player)| player.owner == sender)
        .map(|(name, _)| name.clone())
        .collect();
    // GetAll is only needed for explicitly invalidated properties, not on ordinary notifications.
    if names.is_empty() {
        return;
    }
    let mut properties = properties;
    if !invalidated.is_empty() {
        for name in invalidated {
            let value = match name.as_str() {
                "CanControl" | "CanPlay" | "CanPause" | "CanGoNext" | "CanGoPrevious"
                | "CanSeek" => OwnedValue::from(false),
                "Rate" => OwnedValue::from(0.0f64),
                "Position" => OwnedValue::from(-1i64),
                "Metadata" => Properties::new().into(),
                _ => OwnedValue::try_from(zbus::zvariant::Value::from("")).expect("string value"),
            };
            properties.insert(name, value);
        }
        if let Ok(fresh) = all(connection, &sender, &interface).await {
            properties.extend(fresh);
        }
    }
    let mut names = names.into_iter().peekable();
    while let Some(name) = names.next() {
        let fields = if names.peek().is_none() {
            std::mem::take(&mut properties)
        } else {
            properties
                .iter()
                .filter_map(|(key, value)| value.try_clone().ok().map(|value| (key.clone(), value)))
                .collect()
        };
        let now = Instant::now();
        let mut data = lock(&shared.data);
        if let Some(player) = data.players.get_mut(&name) {
            let was = player.state == PlaybackState::Playing;
            let changed = player.apply(&interface, fields, now);
            data.publish(&name, changed, was, now);
        }
    }
}
fn seeked(shared: &Shared, message: Message) {
    let Some(sender) = message
        .header()
        .sender()
        .map(|sender| sender.as_str().to_owned())
    else {
        return;
    };
    let Ok((position,)) = message.body().deserialize::<(i64,)>() else {
        return;
    };
    let mut data = lock(&shared.data);
    let now = Instant::now();
    let names: Vec<_> = data
        .players
        .iter()
        .filter(|(_, player)| player.owner == sender)
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        if let Some(player) = data.players.get_mut(&name) {
            let was = player.state == PlaybackState::Playing;
            let changed = player.seek(position, now);
            data.publish(&name, changed, was, now);
        }
    }
}
fn dispatch(shared: &Arc<Shared>, connection: &Connection) -> Option<(u64, CommandFuture)> {
    let mut data = lock(&shared.data);
    data.expire(Instant::now());
    let index = data
        .pending
        .iter()
        .position(|pending| !pending.dispatched)?;
    let request = data.pending[index].request;
    let ticket = data.pending[index].ticket;
    let target = data
        .players
        .iter()
        .find(|(_, player)| player.token == request.session)
        .map(|(name, player)| {
            (
                name.clone(),
                player.owner.clone(),
                player.revision,
                player.commands(),
            )
        });
    let (name, target, revision, commands) = match target {
        Some(target) => target,
        None => {
            data.complete(ticket, Err(BackendError::DeviceGone));
            return None;
        }
    };
    if revision != request.expected_revision {
        data.complete(ticket, Err(BackendError::StaleRevision));
        return None;
    }
    if !commands.contains(request.command) {
        data.complete(ticket, Err(BackendError::Unsupported));
        return None;
    }
    let deadline = data.pending[index].deadline;
    data.pending[index].dispatched = true;
    let connection = connection.clone();
    let shared = shared.clone();
    Some((
        ticket,
        async move {
            let result = async {
                if owner(&connection, &name).await? != target {
                    return Err(BackendError::DeviceGone);
                }
                let seek_target = {
                    let mut data = lock(&shared.data);
                    data.expire(Instant::now());
                    if shared.stop.load(Ordering::Acquire)
                        || !data.pending.iter().any(|pending| pending.ticket == ticket)
                    {
                        return Err(BackendError::Cancelled);
                    }
                    let player = data
                        .players
                        .get(&name)
                        .filter(|player| player.token == request.session && player.owner == target)
                        .ok_or(BackendError::DeviceGone)?;
                    if player.revision != request.expected_revision {
                        return Err(BackendError::StaleRevision);
                    }
                    if !player.commands().contains(request.command) {
                        return Err(BackendError::Unsupported);
                    }
                    match request.command {
                        MediaCommand::SeekTo { position_ms } => {
                            Some(player.seek_target(position_ms)?)
                        }
                        _ => None,
                    }
                };
                if let Some((track, position)) = seek_target {
                    return bounded(deadline.saturating_duration_since(Instant::now()), async {
                        Proxy::new(&connection, target.as_str(), PATH, PLAYER)
                            .await?
                            .call::<_, _, ()>("SetPosition", &(track, position))
                            .await
                    })
                    .await;
                }
                let method = match request.command {
                    MediaCommand::Play => "Play",
                    MediaCommand::Pause => "Pause",
                    MediaCommand::Toggle => "PlayPause",
                    MediaCommand::Next => "Next",
                    MediaCommand::Previous => "Previous",
                    MediaCommand::SeekTo { .. } => unreachable!("seek dispatched above"),
                };
                bounded(deadline.saturating_duration_since(Instant::now()), async {
                    Proxy::new(&connection, target.as_str(), PATH, PLAYER)
                        .await?
                        .call::<_, _, ()>(method, &())
                        .await
                })
                .await
            }
            .await;
            (ticket, result)
        }
        .boxed(),
    ))
}
async fn poll_position(connection: &Connection, shared: &Shared) {
    let target = {
        let mut data = lock(&shared.data);
        if !data.pending.is_empty() {
            return;
        }
        let now = Instant::now();
        data.players
            .iter_mut()
            .filter(|(_, player)| player.state == PlaybackState::Playing && now >= player.next_poll)
            .min_by_key(|(_, player)| player.next_poll)
            .map(|(name, player)| {
                player.next_poll = now + Duration::from_secs(1);
                (name.clone(), player.owner.clone())
            })
    };
    let Some((name, owner)) = target else {
        return;
    };
    let result: Result<OwnedValue, _> = bounded(RPC_TIMEOUT, async {
        Proxy::new(connection, owner.as_str(), PATH, PROPERTIES)
            .await?
            .call("Get", &(PLAYER, "Position"))
            .await
    })
    .await;
    if let Ok(position) =
        result.and_then(|value| i64::try_from(value).map_err(|_| BackendError::InvalidArgument))
    {
        let now = Instant::now();
        let mut data = lock(&shared.data);
        if let Some(player) = data
            .players
            .get_mut(&name)
            .filter(|player| player.owner == owner)
        {
            let changed = player.seek(position, now);
            data.publish(&name, changed, true, now);
        }
    }
}
async fn run(shared: Arc<Shared>, ready: mpsc::SyncSender<()>) -> Result<(), BackendError> {
    let connection = bounded(Duration::from_secs(1), Connection::session()).await?;
    let mut owners = stream(&connection, BUS, "NameOwnerChanged", BUS_PATH, true).await?;
    let mut properties = stream(&connection, PROPERTIES, "PropertiesChanged", PATH, false).await?;
    let mut seeks = stream(&connection, PLAYER, "Seeked", PATH, false).await?;
    lock(&shared.data).available = true;
    let _ = ready.send(());
    let mut names: Vec<String> = bounded(RPC_TIMEOUT, async {
        bus_proxy(&connection).await?.call("ListNames", &()).await
    })
    .await?;
    names.retain(|name| name.starts_with(PREFIX));
    names.sort();
    if names.len() > MAX_PLAYERS {
        lock(&shared.data).overflow = true;
    }
    names.truncate(MAX_PLAYERS);
    for name in names {
        discover(&connection, &shared, name, None).await;
    }
    let mut in_flight = FuturesUnordered::new();
    let mut current = None;
    enum Wake {
        Owner(Option<zbus::Result<Message>>),
        Properties(Option<zbus::Result<Message>>),
        Seek(Option<zbus::Result<Message>>),
        Completed((u64, Result<(), BackendError>)),
        Tick,
    }
    while !shared.stop.load(Ordering::Acquire) {
        {
            let mut data = lock(&shared.data);
            data.expire(Instant::now());
            if current.is_some_and(|id| !data.pending.iter().any(|pending| pending.ticket == id)) {
                in_flight.clear();
                current = None;
            }
        }
        if in_flight.is_empty() {
            if let Some((id, future)) = dispatch(&shared, &connection) {
                current = Some(id);
                in_flight.push(future);
            }
        }
        let wake = {
            let completion = async {
                if in_flight.is_empty() {
                    futures::future::pending().await
                } else {
                    in_flight.next().await.expect("nonempty")
                }
            }
            .fuse();
            let tick = futures_timer::Delay::new(Duration::from_millis(10)).fuse();
            futures::pin_mut!(completion, tick);
            futures::select_biased! {message=owners.next().fuse()=>Wake::Owner(message),message=properties.next().fuse()=>Wake::Properties(message),message=seeks.next().fuse()=>Wake::Seek(message),result=completion=>Wake::Completed(result),_=tick=>Wake::Tick}
        };
        match wake {
            Wake::Owner(Some(Ok(message))) => {
                if let Ok((name, _, new)) = message.body().deserialize::<(String, String, String)>()
                {
                    if name.starts_with(PREFIX) {
                        if new.is_empty() {
                            lock(&shared.data).remove(&name);
                        } else {
                            discover(&connection, &shared, name, Some(new)).await;
                        }
                    }
                }
            }
            Wake::Properties(Some(Ok(message))) => changed(&connection, &shared, message).await,
            Wake::Seek(Some(Ok(message))) => seeked(&shared, message),
            Wake::Completed((id, result)) => {
                let mut data = lock(&shared.data);
                data.expire(Instant::now());
                data.complete(id, result);
                current = None;
            }
            Wake::Tick => poll_position(&connection, &shared).await,
            _ => return Err(BackendError::ServerUnavailable),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests;
