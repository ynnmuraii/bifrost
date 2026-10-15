//! The edge-effect overlay: a click-through `zwlr_layer_shell_v1` surface that shows the
//! [`edgefx`] strip while an [`Effect`] is active (wlroots compositors, KWin; GNOME has no
//! layer shell, so [`EdgeOverlay::spawn`] returns `None` there).
//!
//! # Threading
//!
//! The overlay owns one `std` thread with its own Wayland connection and event queue; the
//! caller's runtime is never involved. [`EdgeOverlay`] is only a handle: an `mpsc` sender plus
//! an eventfd that wakes the thread. The thread body is wrapped in `catch_unwind`; a panic
//! just ends the overlay (later `send`s become no-ops).
//!
//! The loop is `dispatch → step/draw → flush → prepare_read → poll → read`. `poll` waits on
//! the Wayland socket and the wake fd with no timeout while the effect is idle, and with the
//! rest of the frame interval while it is active (see [`poll_timeout`]).
//!
//! # Surface lifecycle
//!
//! The surface exists only while the effect is active: it is created on the first active
//! tick and destroyed together with its shm pool and pixel scratch as soon as the effect goes
//! idle. Two `ARGB8888` buffers share one memfd pool and are reused between frames; a frame
//! is skipped when neither buffer has been released yet. The memfd is written with `pwrite`
//! rather than mapped, which keeps this crate free of `unsafe`.
//!
//! # Placement
//!
//! A layer surface anchored to a single edge is *centred* along that edge, so the strip is
//! anchored to the corner of its edge (e.g. left edge: `LEFT | TOP`) and moved along the edge
//! with a margin (`top = strip.start`); see [`Layout::of`]. The surface is created at scale 1:
//! the compositor scales it on HiDPI outputs. The output is always the compositor's default.

use std::error::Error;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefx::{Canvas, Edge, Effect, Strip, Update};
use rustix::event::{eventfd, poll, EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::fs::{memfd_create, MemfdFlags};
use rustix::io::Errno;
use wayland_client::backend::{ReadEventsGuard, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// Layer-shell namespace of the surface.
const NAMESPACE: &str = "multideskflow-edgefx";
/// Minimum time between two effect steps: just over `1 / edgefx::FRAME_S`, i.e. <= 30 fps.
const FRAME: Duration = Duration::from_micros(33_334);
/// Longest `dt` handed to [`Effect::step`] (a stalled thread must not jump the animation).
const MAX_DT_S: f32 = 0.1;
/// How long [`EdgeOverlay::spawn`] waits for the thread to connect and bind its globals.
const READY_TIMEOUT: Duration = Duration::from_secs(3);
/// Pause before the surface is created again after the compositor closed it.
const REOPEN_BACKOFF: Duration = Duration::from_secs(1);
/// Buffers per pool.
const BUFFERS: usize = 2;

type BoxError = Box<dyn Error>;

enum Command {
    Update(Update),
    SetEnabled(bool),
    Quit,
}

/// Wakes the overlay thread. `pending` coalesces notifications so a burst of updates costs one
/// `write`.
struct Wake {
    fd: OwnedFd,
    pending: AtomicBool,
}

impl Wake {
    fn new() -> io::Result<Self> {
        let fd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self {
            fd,
            pending: AtomicBool::new(false),
        })
    }

    /// Call *after* the command has been queued.
    fn notify(&self) {
        if !self.pending.swap(true, Ordering::SeqCst) {
            // EAGAIN means the counter is saturated: the thread is already due to wake up.
            loop {
                match rustix::io::write(&self.fd, &1u64.to_ne_bytes()) {
                    Err(Errno::INTR) => continue,
                    _ => break,
                }
            }
        }
    }

    /// Call *before* draining the command queue.
    fn drain(&self) {
        let mut counter = [0u8; 8];
        let _ = rustix::io::read(&self.fd, &mut counter);
        self.pending.store(false, Ordering::SeqCst);
    }
}

/// Handle to the overlay thread. Dropping it stops the thread and removes the surface.
pub struct EdgeOverlay {
    tx: Sender<Command>,
    wake: Arc<Wake>,
}

impl EdgeOverlay {
    /// Starts the overlay thread. `None` when there is no Wayland connection or the compositor
    /// has no `zwlr_layer_shell_v1` (GNOME): a silent no-op, logged once at debug level.
    ///
    /// Blocks for the connection setup (one registry round trip, at most a few seconds), so
    /// call it once at startup, not per event.
    #[must_use]
    pub fn spawn() -> Option<Self> {
        let wake = match Wake::new() {
            Ok(wake) => Arc::new(wake),
            Err(why) => {
                tracing::debug!("edge overlay disabled: eventfd failed: {why}");
                return None;
            }
        };
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_wake = Arc::clone(&wake);
        let spawned = std::thread::Builder::new()
            .name("edgefx-overlay".into())
            .spawn(move || {
                let body = AssertUnwindSafe(|| run(&rx, &thread_wake, ready_tx));
                if catch_unwind(body).is_err() {
                    tracing::error!("edge overlay thread panicked; overlay disabled");
                }
            });
        if let Err(why) = spawned {
            tracing::debug!("edge overlay disabled: cannot spawn thread: {why}");
            return None;
        }
        // On failure or timeout `overlay` is dropped here, which tells the thread to quit.
        let overlay = Self { tx, wake };
        matches!(ready_rx.recv_timeout(READY_TIMEOUT), Ok(true)).then_some(overlay)
    }

    /// Feeds one message from the core. Non-blocking; the display is always the compositor's
    /// default output (the update carries no output id to map).
    pub fn send(&self, update: Update) {
        self.post(Command::Update(update));
    }

    /// `false` clears the effect at once and ignores updates until it is switched on again.
    pub fn set_enabled(&self, enabled: bool) {
        self.post(Command::SetEnabled(enabled));
    }

    fn post(&self, command: Command) {
        // A closed channel means the thread ended (panic or dead connection): nothing to do.
        if self.tx.send(command).is_ok() {
            self.wake.notify();
        }
    }
}

impl Drop for EdgeOverlay {
    fn drop(&mut self) {
        self.post(Command::Quit);
    }
}

/// Thread body: connect, report readiness, then serve until told to quit or the connection dies.
fn run(rx: &Receiver<Command>, wake: &Wake, ready: SyncSender<bool>) {
    let mut app = match App::connect() {
        Ok(app) => app,
        Err(why) => {
            tracing::debug!("edge overlay disabled: {why}");
            let _ = ready.send(false);
            return;
        }
    };
    let _ = ready.send(true);
    drop(ready);
    if let Err(why) = app.run_loop(rx, wake) {
        tracing::warn!("edge overlay stopped: {why}");
    }
    app.destroy_surface();
    let _ = app.queue.flush();
}

// ---------------------------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------------------------

/// Layer-shell parameters of a strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    anchor: Anchor,
    /// Surface size (width, height) in surface-local px.
    size: (u32, u32),
    /// `top, right, bottom, left`, the order of `set_margin`.
    margin: [i32; 4],
}

impl Layout {
    /// A single-edge anchor would centre the surface along the edge, so the strip is anchored
    /// to the edge's start corner and shifted along the edge by `strip.start` via the margin.
    fn of(strip: &Strip) -> Self {
        let along = strip.start.max(0.0).round() as i32;
        let (anchor, margin) = match strip.edge {
            Edge::Left => (Anchor::Left | Anchor::Top, [along, 0, 0, 0]),
            Edge::Right => (Anchor::Right | Anchor::Top, [along, 0, 0, 0]),
            Edge::Top => (Anchor::Top | Anchor::Left, [0, 0, 0, along]),
            Edge::Bottom => (Anchor::Bottom | Anchor::Left, [0, 0, 0, along]),
        };
        Self {
            anchor,
            size: strip.size_px(1.0),
            margin,
        }
    }

    /// Whether the same surface and buffers can serve `other` (only the margin may differ).
    fn same_shape(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.size == other.size
    }
}

/// How long `poll` may block: forever while idle, else what is left of the frame interval.
fn poll_timeout(active: bool, since_step: Duration) -> Option<Duration> {
    active.then(|| FRAME.saturating_sub(since_step))
}

/// Configure events and margin updates must obey the same cap as animation steps.
fn draw_ready(last_draw: Option<Instant>, now: Instant) -> bool {
    last_draw.is_none_or(|last| now.saturating_duration_since(last) >= FRAME)
}

fn to_timespec(d: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}

// ---------------------------------------------------------------------------------------------
// Wayland state
// ---------------------------------------------------------------------------------------------

/// Bound globals plus what the event handlers report back to the loop.
struct State {
    compositor: WlCompositor,
    shm: WlShm,
    shell: ZwlrLayerShellV1,
    /// Generation of the live surface; events from older ones are ignored.
    generation: u32,
    configured: bool,
    closed: bool,
    /// A frame should be drawn although no step is due (first frame, margin change).
    draw_due: bool,
    busy: [bool; BUFFERS],
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ZwlrLayerShellV1);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, (u32, usize)> for State {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        &(generation, index): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) && generation == state.generation {
            state.busy[index] = false;
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &generation: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if generation != state.generation {
            return;
        }
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                state.configured = true;
                state.draw_due = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

/// The live layer surface with its pool and pixel scratch; all of it is freed on [`destroy`].
///
/// [`destroy`]: Surface::destroy
struct Surface {
    wl_surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    pool: WlShmPool,
    buffers: [WlBuffer; BUFFERS],
    file: File,
    layout: Layout,
    /// Straight-from-`Effect::draw` pixels, reused every frame.
    px: Vec<u32>,
    /// `px` as little-endian bytes (`ARGB8888`), reused every frame.
    bytes: Vec<u8>,
    next: usize,
}

impl Surface {
    fn create(
        state: &mut State,
        qh: &QueueHandle<State>,
        layout: Layout,
    ) -> Result<Self, BoxError> {
        state.generation = state.generation.wrapping_add(1);
        state.configured = false;
        state.closed = false;
        state.draw_due = false;
        state.busy = [false; BUFFERS];
        let generation = state.generation;

        let (w, h) = layout.size;
        let (width, height) = (i32::try_from(w)?, i32::try_from(h)?);
        let stride = width.checked_mul(4).ok_or("stride overflow")?;
        let frame_len = usize::try_from(stride)? * usize::try_from(height)?;
        let pool_len = frame_len * BUFFERS;

        let file = File::from(memfd_create(NAMESPACE, MemfdFlags::CLOEXEC)?);
        file.set_len(pool_len as u64)?;
        let pool = state
            .shm
            .create_pool(file.as_fd(), i32::try_from(pool_len)?, qh, ());
        let mut buffers = Vec::with_capacity(BUFFERS);
        for index in 0..BUFFERS {
            let offset = i32::try_from(index * frame_len)?;
            buffers.push(pool.create_buffer(
                offset,
                width,
                height,
                stride,
                wl_shm::Format::Argb8888,
                qh,
                (generation, index),
            ));
        }
        let Ok(buffers) = <[WlBuffer; BUFFERS]>::try_from(buffers) else {
            return Err("buffer count mismatch".into());
        };

        let wl_surface = state.compositor.create_surface(qh, ());
        let layer = state.shell.get_layer_surface(
            &wl_surface,
            None,
            Layer::Overlay,
            NAMESPACE.to_owned(),
            qh,
            generation,
        );
        layer.set_size(w, h);
        layer.set_anchor(layout.anchor);
        let [top, right, bottom, left] = layout.margin;
        layer.set_margin(top, right, bottom, left);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // An empty input region (never given a rectangle): every pointer event falls through.
        let region = state.compositor.create_region(qh, ());
        wl_surface.set_input_region(Some(&region));
        region.destroy();
        // The first commit carries no buffer; the compositor answers with `configure`.
        wl_surface.commit();

        Ok(Self {
            wl_surface,
            layer,
            pool,
            buffers,
            file,
            layout,
            px: vec![0; usize::try_from(w)? * usize::try_from(h)?],
            bytes: vec![0; frame_len],
            next: 0,
        })
    }

    fn set_margin(&mut self, margin: [i32; 4]) {
        let [top, right, bottom, left] = margin;
        self.layer.set_margin(top, right, bottom, left);
        self.layout.margin = margin;
    }

    /// Rasterizes the effect into a free buffer and commits it. `Ok(false)`: both buffers are
    /// still held by the compositor, the frame is skipped.
    fn draw(&mut self, state: &mut State, effect: &Effect) -> io::Result<bool> {
        let other = (self.next + 1) % BUFFERS;
        let Some(index) = [self.next, other].into_iter().find(|&i| !state.busy[i]) else {
            return Ok(false);
        };
        let (w, h) = self.layout.size;
        effect.draw(&mut Canvas::new(&mut self.px, w as usize, h as usize), 1.0);
        let (words, _) = self.bytes.as_chunks_mut::<4>();
        for (dst, px) in words.iter_mut().zip(&self.px) {
            *dst = px.to_le_bytes();
        }
        self.file
            .write_all_at(&self.bytes, (index * self.bytes.len()) as u64)?;
        self.wl_surface.attach(Some(&self.buffers[index]), 0, 0);
        self.wl_surface.damage(0, 0, w as i32, h as i32);
        self.wl_surface.commit();
        state.busy[index] = true;
        self.next = other;
        Ok(true)
    }

    fn destroy(self) {
        for buffer in &self.buffers {
            buffer.destroy();
        }
        self.pool.destroy();
        self.layer.destroy();
        self.wl_surface.destroy();
    }
}

struct App {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    surface: Option<Surface>,
    effect: Effect,
    enabled: bool,
    /// When the effect was last stepped (or became active); `None` while idle.
    last_step: Option<Instant>,
    /// Last submitted frame, retained across surface replacement to prevent bursts.
    last_draw: Option<Instant>,
    /// No surface is created before this instant (after the compositor closed one).
    reopen_at: Option<Instant>,
}

impl App {
    fn connect() -> Result<Self, BoxError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let compositor = globals.bind::<WlCompositor, _, _>(&qh, 1..=1, ())?;
        let shm = globals.bind::<WlShm, _, _>(&qh, 1..=1, ())?;
        let shell = globals
            .bind::<ZwlrLayerShellV1, _, _>(&qh, 1..=1, ())
            .map_err(|why| format!("zwlr_layer_shell_v1 unavailable: {why}"))?;
        Ok(Self {
            queue,
            qh,
            state: State {
                compositor,
                shm,
                shell,
                generation: 0,
                configured: false,
                closed: false,
                draw_due: false,
                busy: [false; BUFFERS],
            },
            surface: None,
            effect: Effect::new(std::process::id()),
            enabled: true,
            last_step: None,
            last_draw: None,
            reopen_at: None,
        })
    }

    fn run_loop(&mut self, rx: &Receiver<Command>, wake: &Wake) -> Result<(), BoxError> {
        loop {
            wake.drain();
            if !self.apply_commands(rx) {
                return Ok(());
            }
            self.queue.dispatch_pending(&mut self.state)?;
            self.tick(Instant::now())?;
            let write_blocked = match self.queue.flush() {
                Ok(()) => false,
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => true,
                Err(e) => return Err(e.into()),
            };
            // `None`: events arrived meanwhile, dispatch them first.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let since = self.last_step.map_or(Duration::ZERO, |t| {
                Instant::now().saturating_duration_since(t)
            });
            let timeout = poll_timeout(self.effect.is_active(), since);
            wait(guard, wake, timeout, write_blocked)?;
        }
    }

    /// Returns `false` when the overlay must stop.
    fn apply_commands(&mut self, rx: &Receiver<Command>) -> bool {
        loop {
            match rx.try_recv() {
                Ok(Command::Update(update)) => {
                    if self.enabled {
                        self.effect.update(update);
                    }
                }
                Ok(Command::SetEnabled(enabled)) => {
                    self.enabled = enabled;
                    if !enabled {
                        self.effect.clear();
                    }
                }
                Ok(Command::Quit) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => return true,
            }
        }
    }

    /// Steps the effect when a frame is due, keeps the surface in line with it and draws.
    fn tick(&mut self, now: Instant) -> Result<(), BoxError> {
        if self.state.closed {
            self.state.closed = false;
            self.destroy_surface();
            self.reopen_at = Some(now + REOPEN_BACKOFF);
        }
        if !self.effect.is_active() {
            self.go_idle();
            return Ok(());
        }
        let since = match self.last_step {
            Some(t) => now.saturating_duration_since(t),
            None => {
                self.last_step = Some(now);
                Duration::ZERO
            }
        };
        let mut stepped = false;
        if since >= FRAME {
            let still_active = self.effect.step(since.as_secs_f32().min(MAX_DT_S));
            self.last_step = Some(now);
            if !still_active {
                self.go_idle();
                return Ok(());
            }
            stepped = true;
        }
        let Some(strip) = self.effect.strip() else {
            self.go_idle();
            return Ok(());
        };
        self.sync_surface(&strip, now)?;
        self.state.draw_due |= stepped;
        if self.state.configured && self.state.draw_due && draw_ready(self.last_draw, now) {
            if let Some(surface) = &mut self.surface {
                if surface.draw(&mut self.state, &self.effect)? {
                    self.last_draw = Some(now);
                    self.state.draw_due = false;
                }
            }
        }
        Ok(())
    }

    fn go_idle(&mut self) {
        self.last_step = None;
        self.destroy_surface();
    }

    /// Creates the surface, or re-places/re-creates it when the strip moved.
    fn sync_surface(&mut self, strip: &Strip, now: Instant) -> Result<(), BoxError> {
        let layout = Layout::of(strip);
        if let Some(surface) = &mut self.surface {
            if surface.layout.same_shape(&layout) {
                if surface.layout.margin != layout.margin {
                    surface.set_margin(layout.margin);
                    self.state.draw_due = true;
                }
                return Ok(());
            }
        }
        self.destroy_surface();
        if self.reopen_at.is_some_and(|t| now < t) {
            return Ok(());
        }
        self.surface = Some(Surface::create(&mut self.state, &self.qh, layout)?);
        Ok(())
    }

    fn destroy_surface(&mut self) {
        if let Some(surface) = self.surface.take() {
            surface.destroy();
            self.state.configured = false;
        }
    }
}

/// Blocks in `poll` until the Wayland socket or the wake fd is ready, or `timeout` passes, then
/// reads the socket if it is ready.
fn wait(
    guard: ReadEventsGuard,
    wake: &Wake,
    timeout: Option<Duration>,
    write_blocked: bool,
) -> Result<(), BoxError> {
    let wl_fd = guard.connection_fd();
    let wl_flags = if write_blocked {
        PollFlags::IN | PollFlags::OUT
    } else {
        PollFlags::IN
    };
    let mut fds = [
        PollFd::new(&wl_fd, wl_flags),
        PollFd::new(&wake.fd, PollFlags::IN),
    ];
    let timeout = timeout.map(to_timespec);
    match poll(&mut fds, timeout.as_ref()) {
        Ok(_) => {}
        // Dropping `guard` cancels the read.
        Err(Errno::INTR) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let socket_ready = fds[0]
        .revents()
        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
    if !socket_ready {
        return Ok(());
    }
    match guard.read() {
        Ok(_) => Ok(()),
        Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(edge: Edge, start: f32, len: f32) -> Strip {
        Strip { edge, start, len }
    }

    #[test]
    fn left_and_right_edges_anchor_to_the_top_corner_and_shift_down() {
        let left = Layout::of(&strip(Edge::Left, 120.0, 400.0));
        assert_eq!(left.anchor, Anchor::Left | Anchor::Top);
        assert_eq!(left.margin, [120, 0, 0, 0]);
        assert_eq!(left.size, (36, 400));

        let right = Layout::of(&strip(Edge::Right, 120.0, 400.0));
        assert_eq!(right.anchor, Anchor::Right | Anchor::Top);
        assert_eq!(right.margin, [120, 0, 0, 0]);
        assert_eq!(right.size, (36, 400));
    }

    #[test]
    fn top_and_bottom_edges_anchor_to_the_left_corner_and_shift_right() {
        let top = Layout::of(&strip(Edge::Top, 64.0, 900.0));
        assert_eq!(top.anchor, Anchor::Top | Anchor::Left);
        assert_eq!(top.margin, [0, 0, 0, 64]);
        assert_eq!(top.size, (900, 36));

        let bottom = Layout::of(&strip(Edge::Bottom, 64.0, 900.0));
        assert_eq!(bottom.anchor, Anchor::Bottom | Anchor::Left);
        assert_eq!(bottom.margin, [0, 0, 0, 64]);
        assert_eq!(bottom.size, (900, 36));
    }

    #[test]
    fn margin_is_rounded_and_never_negative() {
        assert_eq!(Layout::of(&strip(Edge::Left, 10.6, 50.0)).margin[0], 11);
        assert_eq!(Layout::of(&strip(Edge::Left, -5.0, 50.0)).margin[0], 0);
        assert_eq!(Layout::of(&strip(Edge::Top, f32::NAN, 50.0)).margin[3], 0);
    }

    #[test]
    fn size_matches_the_buffer_edgefx_draws_at_scale_one() {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let s = strip(edge, 0.0, 333.3);
            assert_eq!(Layout::of(&s).size, s.size_px(1.0));
        }
    }

    #[test]
    fn moving_along_the_edge_keeps_the_shape_but_resizing_does_not() {
        let a = Layout::of(&strip(Edge::Left, 0.0, 400.0));
        let moved = Layout::of(&strip(Edge::Left, 90.0, 400.0));
        let longer = Layout::of(&strip(Edge::Left, 0.0, 500.0));
        let other_edge = Layout::of(&strip(Edge::Right, 0.0, 400.0));
        assert!(a.same_shape(&moved));
        assert!(!a.same_shape(&longer));
        assert!(!a.same_shape(&other_edge));
    }

    #[test]
    fn idle_polls_forever() {
        assert_eq!(poll_timeout(false, Duration::ZERO), None);
        assert_eq!(poll_timeout(false, Duration::from_secs(5)), None);
    }

    #[test]
    fn active_polls_for_the_rest_of_the_frame() {
        assert_eq!(poll_timeout(true, Duration::ZERO), Some(FRAME));
        assert_eq!(
            poll_timeout(true, Duration::from_millis(10)),
            Some(FRAME - Duration::from_millis(10))
        );
        assert_eq!(poll_timeout(true, FRAME), Some(Duration::ZERO));
        assert_eq!(
            poll_timeout(true, Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn frame_interval_is_at_most_thirty_fps() {
        assert!(FRAME >= Duration::from_secs_f32(edgefx::FRAME_S));
        assert!(FRAME < Duration::from_millis(34));
    }

    #[test]
    fn configure_and_margin_redraws_cannot_exceed_the_frame_cap() {
        let now = Instant::now();
        assert!(draw_ready(None, now));
        assert!(!draw_ready(Some(now), now));
        let just_before_frame = now + FRAME - Duration::from_nanos(1);
        assert!(!draw_ready(Some(now), just_before_frame));
        assert!(draw_ready(Some(now), now + FRAME));
    }

    #[test]
    fn timespec_keeps_seconds_and_nanoseconds() {
        let ts = to_timespec(Duration::new(2, 33_000_000));
        assert_eq!((ts.tv_sec, ts.tv_nsec), (2, 33_000_000));
    }

    fn eventfd_counter(wake: &Wake) -> Option<u64> {
        let mut buf = [0u8; 8];
        rustix::io::read(&wake.fd, &mut buf)
            .ok()
            .map(|_| u64::from_ne_bytes(buf))
    }

    #[test]
    fn wake_coalesces_notifications_until_drained() {
        let wake = Wake::new().expect("eventfd");
        wake.notify();
        wake.notify();
        wake.notify();
        // One write, however many notifications.
        assert_eq!(eventfd_counter(&wake), Some(1));
        // Still pending: no further write until the thread drained.
        wake.notify();
        assert_eq!(eventfd_counter(&wake), None);
        wake.drain();
        wake.notify();
        assert_eq!(eventfd_counter(&wake), Some(1));
    }

    #[test]
    fn handle_queues_commands_and_wakes_the_thread() {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(Wake::new().expect("eventfd"));
        let overlay = EdgeOverlay {
            tx,
            wake: Arc::clone(&wake),
        };
        overlay.send(Update::Clear);
        overlay.set_enabled(false);
        assert!(matches!(rx.try_recv(), Ok(Command::Update(Update::Clear))));
        assert!(matches!(rx.try_recv(), Ok(Command::SetEnabled(false))));
        assert_eq!(eventfd_counter(&wake), Some(1));
        drop(overlay);
        assert!(matches!(rx.try_recv(), Ok(Command::Quit)));
    }

    #[test]
    fn send_after_the_thread_ended_is_a_silent_no_op() {
        let (tx, rx) = mpsc::channel();
        let overlay = EdgeOverlay {
            tx,
            wake: Arc::new(Wake::new().expect("eventfd")),
        };
        drop(rx);
        overlay.send(Update::Left);
        overlay.set_enabled(true);
    }
}
