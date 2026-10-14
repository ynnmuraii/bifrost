//! What one frame of the window costs on the CPU, with the field off and with it moving.
//!
//! ```text
//! cargo run --release -p lws-gui --example frame_cost
//! ```
//!
//! No window and no GPU: a frame is `ui::draw` plus egui's tessellation, which is everything the
//! CPU does per frame short of the driver. It also counts the heap allocations of a frame (a
//! counting global allocator), which is the «allocation-free per frame» check of the field.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lws_gui::egui;
use lws_gui::gui_settings::{FieldMode, Look, Pace};
use lws_gui::ui::{self, Env, UiState};
use lws_gui::view_model::{CoreEndpoint, LocalConfig, ViewModel};
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's contract is passed through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn main() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let mut vm = ViewModel::new(
        CoreEndpoint::Ready {
            description: "pipe".to_owned(),
        },
        LocalConfig::default(),
    );
    vm.on_connected("pipe");
    vm.apply(GuiFrame {
        seq: 1,
        request_id: 0,
        msg: Some(Msg::Status(protocol::ipc::Status::default())),
    });
    for (label, field, pace, clouds) in [
        ("field off          ", FieldMode::Off, Pace::Calm, true),
        ("repel calm, clouds ", FieldMode::Repel, Pace::Calm, true),
        ("repel live, clouds ", FieldMode::Repel, Pace::Live, true),
        ("repel calm, none   ", FieldMode::Repel, Pace::Calm, false),
    ] {
        vm.look = Look {
            field,
            pace,
            clouds,
            ..Look::default()
        };
        let mut state = UiState::new(Instant::now());
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let mut frame = |n: u32| {
            let input = egui::RawInput {
                screen_rect: Some(rect),
                time: Some(f64::from(n) / 30.0),
                events: vec![egui::Event::PointerMoved(egui::pos2(
                    300.0 + (n % 400) as f32,
                    300.0,
                ))],
                ..Default::default()
            };
            let env = Env {
                now: Instant::now(),
                animate: true,
            };
            let output = ctx.run_ui(input, |ui| {
                let _ = ui::draw(ui, &vm, &mut state, env);
            });
            let meshes = ctx.tessellate(output.shapes.clone(), output.pixels_per_point);
            std::hint::black_box(&meshes);
            output.drop_without_applying_deltas();
        };
        for n in 0..60 {
            frame(n);
        }
        let allocs = ALLOCS.load(Ordering::Relaxed);
        let start = Instant::now();
        let frames = 600;
        for n in 0..frames {
            frame(60 + n);
        }
        let elapsed = start.elapsed();
        let allocs = ALLOCS.load(Ordering::Relaxed) - allocs;
        println!(
            "{label} {:>7.1} µs/frame  {:>7} allocs/frame  (particles {}, cloud cells {})",
            elapsed.as_secs_f64() * 1e6 / f64::from(frames),
            allocs / u64::from(frames),
            state.field.particles(),
            state.field.cloud_cells(),
        );
    }
}
