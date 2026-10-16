//! Counts only the field's work, not egui input/layout/tessellation allocations.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lws_gui::egui;
use lws_gui::gui_settings::{FieldMode, Look, Theme};
use lws_gui::ui::{self, Field, FieldFrame, Palette};

struct Counting;
thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}

fn allocation() {
    let _ = COUNT.try_with(|count| {
        if let Some(n) = count.get() {
            count.set(Some(n + 1));
        }
    });
}

// SAFETY: every operation forwards the allocator contract unchanged to System.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn active_field_and_ascii_links_allocate_nothing_after_warmup() {
    assert_eq!(Look::default().field, FieldMode::Attract);
    for theme in [Theme::Light, Theme::Dark] {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let look = Look {
            theme,
            ..Look::default()
        };
        let pal = Palette::of(theme);
        let mut field = Field::new();
        let mut bridge = ui::AsciiBridge::default();
        for n in 0..180 {
            let t = n as f32 / 30.0;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    time: Some(f64::from(t)),
                    ..Default::default()
                },
                |ui| {
                    let pointer = Some(egui::pos2(300.0 + n as f32, 250.0));
                    let frame = FieldFrame {
                        map: ui.ctx().content_rect(),
                        pal: &pal,
                        look,
                        simulate: true,
                        t,
                        pointer,
                    };
                    if n >= 60 {
                        COUNT.set(Some(0));
                    }
                    field.paint(ui.painter(), &frame);
                    field.link(
                        ui.painter(),
                        egui::pos2(100.0, 200.0),
                        egui::pos2(900.0, 200.0),
                        egui::Color32::WHITE,
                    );
                    bridge.tick(t, true);
                    bridge.set_latency(ui.painter(), ui_text::Lang::Ru, Some(1_500));
                    bridge.paint(
                        ui.painter(),
                        egui::pos2(100.0, 200.0),
                        egui::pos2(900.0, 200.0),
                        true,
                        true,
                        1.5,
                        120.0,
                        pal.accent,
                    );
                    bridge.paint_latency(ui.painter(), egui::pos2(500.0, 200.0), &pal);
                    let allocations = COUNT.replace(None);
                    if let Some(allocations) = allocations {
                        assert_eq!(allocations, 0, "theme={theme:?}, frame={n}");
                    }
                },
            );
            output.drop_without_applying_deltas();
        }
    }
}
