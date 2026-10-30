//! Headless release CPU/allocation report, using virtual time (no sleeps or GPU).
//! cargo run --release -p bifrost-gui --example frame_cost -- --root DIR
//! --root/--live uses a read-only real core snapshot, otherwise an offline fixture.
//! CPU means wall time spent drawing/tessellating, not process-wide CPU accounting.
//! Allocation phases separate app drawing from egui orchestration and tessellation;
//! the direct field probe below excludes all three surrounding framework costs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bifrost_gui::egui;
use bifrost_gui::gui_settings::Look;
use bifrost_gui::ui::{self, Env, Field, FieldFrame, Palette, UiState};
use bifrost_gui::view_model::{self, Bridge, CliArgs, CoreEndpoint, LocalConfig, ViewModel};
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: all operations forward the caller's allocator contract unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn snapshot() -> Result<ViewModel, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--root" || arg == "--live") {
        let (view, client) = view_model::boot(CliArgs::parse(args));
        let mut bridge = Bridge::new(view, client);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            bridge.pump();
            let view = bridge.view();
            if view.connection.is_connected()
                && view.status.is_some()
                && view.editor.has_document()
                && view.inflight.is_empty()
            {
                println!("data_source=IPC topology_revision={}", view.editor.revision);
                return Ok(view.clone());
            }
            if Instant::now() >= deadline {
                return Err(format!("no complete core snapshot: {:?}", view.connection));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let mut view = ViewModel::new(
        CoreEndpoint::Ready {
            description: "offline fixture".to_owned(),
        },
        LocalConfig::default(),
    );
    view.on_connected("offline fixture");
    view.apply(GuiFrame {
        seq: 1,
        request_id: 0,
        msg: Some(Msg::Status(protocol::ipc::Status::default())),
    });
    println!("data_source=offline_fixture");
    Ok(view)
}

fn input(t: Duration, events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1920.0, 1080.0),
        )),
        time: Some(t.as_secs_f64()),
        events,
        ..Default::default()
    }
}

/// How the phases below drive the pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drive {
    /// It moves across the map at 20 Hz.
    Moves,
    /// It arrives over the map once, and then stays exactly where it is.
    Still,
    /// It leaves the map: the window has nothing to animate at all.
    Gone,
}

fn main() -> Result<(), String> {
    let mut vm = snapshot()?;
    // Measure the shipped default, independent of a user's saved appearance.
    vm.look = Look::default();
    vm.modal = None;
    vm.appearance_open = false;
    vm.pairing = Default::default();
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let epoch = Instant::now();
    let mut state = UiState::new(epoch);
    let mut due = Duration::ZERO;
    let mut total = 0u64;
    let mut idle_frames = 0;
    // A bridge keeps flowing under a stationary pointer; the background alone may settle.
    let has_bridges = vm.map_model().links.iter().any(|link| {
        matches!(
            link.state,
            bifrost_gui::view_model::layout::LinkState::Idle
                | bifrost_gui::view_model::layout::LinkState::Active
        )
    });
    for (label, start, end, drive) in [
        ("warmup", 0, 3, Drive::Moves),
        ("active", 3, 8, Drive::Moves),
        ("hold", 8, 13, Drive::Still),
        ("idle", 13, 23, Drive::Gone),
    ] {
        let begin = Duration::from_secs(start);
        let end = Duration::from_secs(end);
        let mut event = begin;
        let mut frames = 0u64;
        let mut repaint_frames = 0u64;
        let mut late_frames = 0u64;
        let mut cpu = Duration::ZERO;
        let mut allocations = 0u64;
        let mut draw_allocations = 0u64;
        let mut tessellation_allocations = 0u64;
        loop {
            let t = due.min(event);
            if t >= end {
                break;
            }
            let is_event = event <= due;
            let events = if is_event {
                match drive {
                    Drive::Moves => {
                        event += Duration::from_millis(50);
                        vec![egui::Event::PointerMoved(egui::pos2(
                            300.0 + (t.as_secs_f32() * 120.0) % 600.0,
                            250.0,
                        ))]
                    }
                    Drive::Still => {
                        event = Duration::MAX;
                        vec![egui::Event::PointerMoved(egui::pos2(900.0, 500.0))]
                    }
                    Drive::Gone => {
                        event = Duration::MAX;
                        vec![egui::Event::PointerGone]
                    }
                }
            } else {
                Vec::new()
            };
            let raw = input(t, events);
            let before = ALLOCS.load(Ordering::Relaxed);
            let began = Instant::now();
            let mut output = ctx.run_ui(raw, |ui| {
                let draw_before = ALLOCS.load(Ordering::Relaxed);
                let now = epoch + t;
                vm.tick(now);
                let drawn = ui::draw(ui, &vm, &mut state, Env { now, animate: true });
                let pending = vm.next_wakeup(now);
                if let Some(after) = drawn.repaint_after.into_iter().chain(pending).min() {
                    ui.ctx().request_repaint_after(after);
                }
                draw_allocations += ALLOCS.load(Ordering::Relaxed) - draw_before;
            });
            let delay = output
                .viewport_output
                .values()
                .map(|v| v.repaint_delay)
                .min()
                .unwrap_or(Duration::MAX);
            let tessellation_before = ALLOCS.load(Ordering::Relaxed);
            let meshes =
                ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
            tessellation_allocations += ALLOCS.load(Ordering::Relaxed) - tessellation_before;
            output.drop_without_applying_deltas();
            std::hint::black_box(&meshes);
            cpu += began.elapsed();
            allocations += ALLOCS.load(Ordering::Relaxed) - before;
            due = if delay == Duration::MAX {
                Duration::MAX
            } else {
                t + delay.max(Duration::from_millis(1))
            };
            frames += 1;
            repaint_frames += u64::from(!is_event);
            late_frames += u64::from(t > begin + Duration::from_secs(1));
            total += 1;
            assert!(total < 100_000, "reactive loop failed to advance");
        }
        if label == "idle" {
            idle_frames = frames;
        }
        if label == "hold" {
            if has_bridges {
                assert!(
                    frames <= 155,
                    "bridge animation exceeds 30 fps plus transition frames: {frames}"
                );
            } else {
                assert!(
                    frames < 100,
                    "a field without bridges must settle: {frames}"
                );
            }
        }
        if label == "idle" {
            // Leaving freezes the last active frame; after the pointer event itself the
            // window stays silent for the rest of the ten virtual seconds.
            assert_eq!(
                late_frames, 0,
                "an idle window must not repaint on a timer: {late_frames} frames after the                  first second"
            );
        }
        let seconds = (end - begin).as_secs_f64();
        println!("{label}: virtual_s={seconds:.1} frames={frames} timed_repaints={repaint_frames} cpu_ms={:.3} cpu_us/frame={:.2} allocs={allocations} allocs/frame={:.2} cpu_ms/virtual_s={:.3}",
            cpu.as_secs_f64() * 1e3,
            cpu.as_secs_f64() * 1e6 / frames.max(1) as f64,
            allocations as f64 / frames.max(1) as f64,
            cpu.as_secs_f64() * 1e3 / seconds);
        println!("{label} allocation phases: ui_draw/frame={:.2} tessellation/frame={:.2} egui_run_and_output/frame={:.2}",
            draw_allocations as f64 / frames.max(1) as f64,
            tessellation_allocations as f64 / frames.max(1) as f64,
            allocations.saturating_sub(draw_allocations + tessellation_allocations) as f64 / frames.max(1) as f64);
    }
    direct_field();
    assert!(
        idle_frames <= 3,
        "the pointer leaving the map is a transition, not a loop: {idle_frames} frames"
    );
    Ok(())
}

fn direct_field() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let look = Look::default();
    let pal = Palette::of(look.theme);
    let mut field = Field::new();
    let mut bridge = ui::AsciiBridge::default();
    let mut allocations = 0;
    let mut cpu = Duration::ZERO;
    for n in 0..660 {
        let t = Duration::from_secs_f64(f64::from(n) / 30.0);
        let output = ctx.run_ui(input(t, Vec::new()), |ui| {
            let frame = FieldFrame {
                map: ui.ctx().content_rect(),
                pal: &pal,
                look: &look,
                simulate: true,
                t: t.as_secs_f32(),
                pointer: Some(egui::pos2(300.0 + (n % 400) as f32, 250.0)),
            };
            let before = ALLOCS.load(Ordering::Relaxed);
            let began = Instant::now();
            field.paint(ui.painter(), &frame);
            bridge.tick(t.as_secs_f32(), true);
            bridge.set_latency(ui.painter(), ui_text::Lang::Ru, Some(1_500));
            bridge.paint(
                ui.painter(),
                egui::pos2(100.0, 200.0),
                egui::pos2(900.0, 200.0),
                true,
                true,
                1.5,
                (120.0, 120.0),
                pal.accent,
            );
            bridge.paint_latency(
                ui.painter(),
                egui::pos2(500.0, 200.0),
                &pal,
                std::iter::empty(),
            );
            if n >= 60 {
                cpu += began.elapsed();
                allocations += ALLOCS.load(Ordering::Relaxed) - before;
            }
        });
        // Keep egui's reused shape buffers, but exclude its work from the direct check.
        output.drop_without_applying_deltas();
    }
    println!("direct Field::paint + ASCII bridge: frames=600 cpu_us/frame={:.2} allocs={allocations} allocs/frame={:.3} particles={} cloud_cells={} (excludes egui run/tessellation)",
        cpu.as_secs_f64() * 1e6 / 600.0, allocations as f64 / 600.0, field.particles(), field.cloud_cells());
}
