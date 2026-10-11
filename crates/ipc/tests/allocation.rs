//! Does a hostile length prefix make the decoder allocate?
//!
//! This file deliberately holds a single test: the counter is process-global, and
//! an integration-test binary runs its tests on parallel threads, so any second
//! test in the same binary could allocate while this one measures.
//!
//! The claim being checked is the one the documents make for the wire
//! («Не выделять память до проверки длины»): a prefix is judged from the four
//! bytes that are present, so a prefix asking for 4 GiB — or for zero — must be
//! rejected with no allocation and with nothing buffered for the body it
//! declared, no matter what the peer sends afterwards.

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use ipc::{Framed, IpcError};
use protocol::FrameError;
use tokio::io::{AsyncWriteExt, DuplexStream};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Drive `future` to completion inside the measured region, so the count covers
/// the whole decode attempt rather than only its first poll.
fn run_to_completion<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    for _ in 0..1000 {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
    panic!("the future did not complete within 1000 polls");
}

/// A waker that never needs to wake anything: the in-memory duplex is ready as
/// soon as the bytes are there.
fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: every callback is a no-op on a null pointer, which is the
    // documented contract for a waker that never wakes.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// A link whose read buffer is already grown by one complete frame, so the
/// measurement below sees the decode path alone and not the first allocation of
/// the buffer.
async fn warmed_link() -> (DuplexStream, Framed<DuplexStream>) {
    let (mut writer, reader) = tokio::io::duplex(64);
    let mut framed = Framed::ipc(reader);
    writer.write_all(&4u32.to_be_bytes()).await.unwrap();
    writer.write_all(&[1, 2, 3, 4]).await.unwrap();
    writer.flush().await.unwrap();
    assert_eq!(framed.recv_bytes().await.unwrap(), [1, 2, 3, 4]);
    assert_eq!(framed.buffered_len(), 0);
    (writer, framed)
}

async fn rejects_without_allocating(prefix: u32, expected: FrameError) {
    let (mut writer, mut framed) = warmed_link().await;
    writer.write_all(&prefix.to_be_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    ALLOCATIONS.store(0, Ordering::SeqCst);
    let result = run_to_completion(framed.recv_bytes());
    let allocations = ALLOCATIONS.load(Ordering::SeqCst);

    match &result {
        Err(IpcError::Framing(framing)) => assert_eq!(framing, &expected, "prefix {prefix}"),
        other => panic!("prefix {prefix} must be a framing error, got {other:?}"),
    }
    assert_eq!(
        allocations, 0,
        "rejecting a {prefix} prefix must not allocate"
    );
    assert_eq!(
        framed.buffered_len(),
        4,
        "only the prefix was buffered, nothing for the declared {prefix}-byte body"
    );
    assert!(framed.buffered_len() < framed.max_buffered_len());
}

#[tokio::test(flavor = "current_thread")]
async fn a_hostile_prefix_allocates_nothing_and_buffers_nothing() {
    // Improbably large: u32::MAX, and one byte above the channel limit.
    rejects_without_allocating(
        u32::MAX,
        FrameError::TooLarge {
            len: u32::MAX as usize,
            max: 65_536,
        },
    )
    .await;
    rejects_without_allocating(
        65_537,
        FrameError::TooLarge {
            len: 65_537,
            max: 65_536,
        },
    )
    .await;
    // And the other end of the range, which must be rejected just as cheaply.
    rejects_without_allocating(0, FrameError::Empty).await;
}
