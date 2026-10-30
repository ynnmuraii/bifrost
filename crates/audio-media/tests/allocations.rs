//! Counts Rust heap traffic only; libopus C allocation behavior is not intercepted.
use audio_media::{
    crypto::{DirectionMaterial, Opener, Sealer},
    pipeline::{Receiver, Sender},
};
use audio_model::jitter::Profile;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
use zeroize::Zeroizing;
struct Counting;
thread_local! { static ENABLED: Cell<bool> = const { Cell::new(false) }; static ALLOCATIONS: Cell<usize> = const { Cell::new(0) }; }
fn count() {
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }
    });
}
// SAFETY: Every operation delegates the unchanged pointer/layout contract to System.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
fn material() -> DirectionMaterial {
    DirectionMaterial::new(Zeroizing::new([1; 32]), Zeroizing::new([2; 4]))
}
#[test]
fn prepared_packet_and_playout_paths_do_not_allocate() {
    for profile in [Profile::Ethernet, Profile::Wifi] {
        let mut sender =
            Sender::new(profile, Sealer::new(material(), [3; 16], 0).unwrap(), 0).unwrap();
        let mut receiver = Receiver::new(
            profile,
            Opener::new(material(), [3; 16]).unwrap(),
            u32::from(profile.frame_samples()),
        )
        .unwrap();
        let pcm = [0.01; 1920];
        let mut packet = [0; 1200];
        let mut output = vec![0.0; receiver.output_capacity_samples()];
        let frame = usize::from(profile.frame_samples());
        for i in 0..110u64 {
            if i == 10 {
                ALLOCATIONS.with(|n| n.set(0));
                ENABLED.with(|enabled| enabled.set(true));
            }
            let now = i * frame as u64;
            let n = sender
                .encode(now / 48, &pcm[..frame * 2], &mut packet)
                .unwrap();
            receiver.receive(now, &mut packet[..n], |_| true).unwrap();
            receiver.playout(now, frame as f64, &mut output).unwrap();
        }
        ENABLED.with(|enabled| enabled.set(false));
        assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    }
}
