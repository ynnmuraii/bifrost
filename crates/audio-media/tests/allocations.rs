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

#[test]
fn dry_capture_and_render_callbacks_do_not_allocate_even_on_overflow_underflow() {
    use audio_backend::{
        dry::{DryBackend, ScriptEvent},
        AudioFormat, Capabilities, CaptureSpec, Device, DeviceId, DeviceSelection, Direction,
        RenderRequest,
    };
    use audio_media::runtime::{start_listener, start_source, SessionConfig, SessionMaterial};
    use std::net::UdpSocket;
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        exclude_own_process_tree: true,
        ..Capabilities::default()
    });
    let id = DeviceId::new("DRY_RENDER").unwrap();
    backend
        .apply(ScriptEvent::UpsertDevice(Device {
            id: id.clone(),
            label: "Dry render".into(),
            direction: Direction::Render,
            format: AudioFormat {
                sample_rate: 44_100,
                channels: 2,
            },
        }))
        .unwrap();
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let aa = a.local_addr().unwrap();
    let ba = b.local_addr().unwrap();
    let keys = || SessionMaterial {
        forward: material(),
        reverse: DirectionMaterial::new(Zeroizing::new([4; 32]), Zeroizing::new([5; 4])),
    };
    let mut source = start_source(
        &mut backend,
        CaptureSpec::SystemExcludingOwnProcessTree {
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            socket: a,
            peer: ba,
            session_id: [3; 16],
            profile: Profile::Ethernet,
        },
        keys(),
    )
    .unwrap();
    let mut listener = start_listener(
        &mut backend,
        RenderRequest {
            device: DeviceSelection::Explicit(id),
            requested: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            socket: b,
            peer: aa,
            session_id: [3; 16],
            profile: Profile::Ethernet,
        },
        keys(),
    )
    .unwrap();
    // Setup/worker allocations occur outside this thread's counted interval.
    ALLOCATIONS.with(|n| n.set(0));
    ENABLED.with(|flag| flag.set(true));
    for _ in 0..100 {
        source.stream_mut().unwrap().pump(480).unwrap();
        listener.stream_mut().unwrap().pump(441).unwrap();
    }
    ENABLED.with(|flag| flag.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    source.stop().unwrap();
    listener.stop().unwrap();
}
