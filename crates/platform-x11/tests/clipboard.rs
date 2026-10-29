//! Real X11 CLIPBOARD transfers through a second, independent X connection.
//!
//! The non-ignored PNG test needs only a display (`xvfb-run -a cargo test -p platform-x11
//! --test clipboard`). The older external-client smoke additionally needs `xclip` and
//! stays ignored. Neither fixture uses the adapter's INCR machines for its raw peer.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clipboard::{ClipboardContent, Marker};
use platform_x11::clipboard::{Clipboard, DEFAULT_MAX_REQUEST_SIZE};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
    EventMask, PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// `xclip -selection clipboard -o`, byte for byte.
fn xclip_out() -> Vec<u8> {
    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-o", "-verbose"])
        .output()
        .expect("xclip runs");
    eprint!(
        "[xclip -o stderr] {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "xclip -o failed: {output:?}");
    output.stdout
}

/// Who the server says owns CLIPBOARD right now, from a third connection.
fn clipboard_owner() -> u32 {
    let (conn, _) = x11rb::connect(None).expect("probe display");
    conn.get_selection_owner(intern(&conn, "CLIPBOARD"))
        .expect("query owner")
        .reply()
        .expect("owner reply")
        .owner
}

/// Runs `xclip -selection clipboard -i` and feeds it `data`.
///
/// xclip forks: the process we wait on takes ownership, hands it to its child and exits, so
/// after this returns the selection belongs to an external client that keeps serving it.
fn xclip_in(data: &[u8]) {
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard", "-i"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("xclip runs");
    child
        .stdin
        .as_mut()
        .expect("xclip stdin is piped")
        .write_all(data)
        .expect("xclip accepts the payload");
    drop(child.stdin.take());
    let status = child.wait().expect("xclip exits");
    assert!(status.success(), "xclip -i failed: {status:?}");
}

/// Drops any already-queued change notifications.
fn drain(clipboard: &Clipboard) {
    while clipboard.wait(Duration::from_millis(0)).is_some() {}
}

fn wait_for_change(clipboard: &Clipboard) -> bool {
    clipboard.wait(Duration::from_secs(10)).is_some()
}

#[test]
#[ignore = "needs a real X display and xclip"]
fn clipboard_round_trips_against_an_external_client() {
    let clipboard = Clipboard::connect().expect("the clipboard adapter connects");
    drain(&clipboard);

    // 1. Out: we own CLIPBOARD, an external client pastes it.
    let out = format!("lws-clipboard-smoke-out-{}", std::process::id());
    clipboard
        .set(ClipboardContent::Text(out.clone()))
        .expect("taking CLIPBOARD");
    println!("owner right after set: {}", clipboard_owner());
    assert_eq!(
        xclip_out(),
        out.as_bytes(),
        "xclip pasted exactly what we published"
    );
    println!("out: xclip -o printed {:?}", out);

    // 2. In: an external client owns CLIPBOARD, we see the change and read it.
    drain(&clipboard);
    let incoming = format!("lws-clipboard-smoke-in-{}", std::process::id());
    xclip_in(incoming.as_bytes());
    assert!(
        wait_for_change(&clipboard),
        "the external write was observed"
    );
    let snapshot = clipboard.read().expect("reading the external selection");
    assert_eq!(snapshot.content, Some(ClipboardContent::Text(incoming)));
    assert!(snapshot.markers.is_empty(), "plain text carries no marker");
    println!("in: read {:?} after a change", snapshot.content);

    // 3. A large text round trip. Outgoing transfer must use INCR; xclip chooses its own
    // incoming threshold, while both sides stay inside the text hard cap.
    let payload_len = clipboard::MAX_BYTES as usize;
    assert!(
        payload_len > DEFAULT_MAX_REQUEST_SIZE,
        "the payload must exceed this adapter's INCR threshold"
    );

    let big: Vec<u8> = (0..payload_len).map(|i| b'a' + (i % 26) as u8).collect();
    let big_text = std::str::from_utf8(&big).expect("the payload is ASCII");

    clipboard
        .set(ClipboardContent::Text(big_text.to_owned()))
        .expect("taking CLIPBOARD with a large payload");
    let pasted = xclip_out();
    assert_eq!(
        pasted.len(),
        payload_len,
        "INCR out: xclip received {} of {payload_len} bytes",
        pasted.len()
    );
    assert_eq!(pasted, big, "INCR out is byte-for-byte");
    println!("INCR out: {} bytes served to xclip -o", pasted.len());

    drain(&clipboard);
    xclip_in(&big);
    assert!(
        wait_for_change(&clipboard),
        "the large external write was observed"
    );
    let snapshot = clipboard
        .read()
        .expect("reading the large external selection");
    let content = snapshot.content.as_ref().expect("xclip published text");
    assert_eq!(
        content.len(),
        payload_len,
        "INCR in: read {} of {payload_len} bytes",
        content.len()
    );
    assert_eq!(content.as_bytes(), &big[..], "INCR in is byte-for-byte");
    println!(
        "INCR in: {} bytes read from xclip's selection",
        content.len()
    );

    // 4. KDE's secret hint, and its absence.
    let secret = "lws-clipboard-kde-secret-item";
    let owner = RawOwner::start(Some(secret), None, Some("secret"));
    assert!(
        wait_for_change(&clipboard),
        "the helper taking CLIPBOARD was observed"
    );
    let snapshot = clipboard.read().expect("reading the helper's selection");
    assert_eq!(
        snapshot.content,
        Some(ClipboardContent::Text(secret.to_owned()))
    );
    assert_eq!(
        snapshot.markers,
        vec![Marker::KdeSecret],
        "x-kde-passwordManagerHint=secret is classified as a secret"
    );
    println!(
        "kde hint: read {:?} with {:?}",
        snapshot.content, snapshot.markers
    );
    drop(owner);

    drain(&clipboard);
    let plain = "lws-clipboard-plain-item";
    xclip_in(plain.as_bytes());
    assert!(wait_for_change(&clipboard), "the plain write was observed");
    let snapshot = clipboard.read().expect("reading the plain selection");
    assert_eq!(
        snapshot.content,
        Some(ClipboardContent::Text(plain.to_owned()))
    );
    assert!(
        snapshot.markers.is_empty(),
        "an item with no secret hint carries no marker"
    );
    println!("plain: read {:?} with no marker", snapshot.content);
}

#[test]
fn png_round_trips_through_independent_x11_clients() {
    let Ok((probe, _)) = x11rb::connect(None) else {
        eprintln!("requires an isolated Xvfb display");
        return;
    };
    drop(probe);
    let clipboard = Clipboard::connect().expect("the clipboard adapter connects");
    let png = large_png();
    assert!(png.len() > DEFAULT_MAX_REQUEST_SIZE);

    // A different owner streams PNG through real X properties, not the pure INCR machine.
    let owner = RawOwner::start(None, Some(png.clone()), None);
    let snapshot = clipboard.read().expect("reading the raw PNG selection");
    let Some(ClipboardContent::Image { png: received }) = snapshot.content else {
        panic!("PNG content expected");
    };
    assert_eq!(received, png);
    assert!(snapshot.markers.is_empty());
    assert_eq!(owner.png_requests.load(Ordering::SeqCst), 1);
    assert_eq!(owner.incr_transfers.load(Ordering::SeqCst), 1);
    drop(owner);

    let owner = RawOwner::start(Some("text wins over PNG"), Some(png.clone()), None);
    let snapshot = clipboard.read().expect("reading mixed targets");
    assert_eq!(
        snapshot.content,
        Some(ClipboardContent::Text("text wins over PNG".to_owned()))
    );
    assert!(snapshot.markers.is_empty());
    assert_eq!(owner.png_requests.load(Ordering::SeqCst), 0);
    drop(owner);

    clipboard
        .set(ClipboardContent::Image { png: png.clone() })
        .expect("owning PNG");
    let (requestor, screen) = x11rb::connect(None).expect("the raw requestor connects");
    let window = raw_window(&requestor, screen);
    let targets = raw_read(&requestor, window, intern(&requestor, "TARGETS"));
    assert_eq!(targets.type_, u32::from(AtomEnum::ATOM));
    assert_eq!(targets.format, 32);
    let (atoms, remainder) = targets.bytes.as_chunks::<4>();
    assert!(remainder.is_empty());
    for name in ["TARGETS", "TIMESTAMP", "image/png"] {
        assert!(
            atoms.contains(&intern(&requestor, name).to_ne_bytes()),
            "{name}"
        );
    }
    for name in ["UTF8_STRING", "TEXT"] {
        assert!(
            !atoms.contains(&intern(&requestor, name).to_ne_bytes()),
            "{name}"
        );
    }
    let png_atom = intern(&requestor, "image/png");
    let received = raw_read(&requestor, window, png_atom);
    assert!(received.incr, "large adapter-owned PNG must use INCR");
    assert!(
        received.chunks > 1,
        "PNG must cross several property writes"
    );
    assert_eq!(received.type_, png_atom);
    assert_eq!(received.format, 8);
    assert_eq!(received.bytes, png);

    let owner = RawOwner::start(None, Some(png), Some("secret"));
    let snapshot = clipboard.read().expect("reading image secret markers");
    assert_eq!(snapshot.markers, [Marker::KdeSecret]);
    assert_eq!(
        snapshot.content,
        Some(ClipboardContent::Text(String::new()))
    );
    assert_eq!(owner.png_requests.load(Ordering::SeqCst), 0);
    assert_eq!(owner.incr_transfers.load(Ordering::SeqCst), 0);
}

fn raw_window(conn: &RustConnection, screen: usize) -> u32 {
    let window = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        conn.setup().roots[screen].root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .unwrap()
    .check()
    .unwrap();
    window
}

fn raw_property(
    conn: &RustConnection,
    window: u32,
    property: Atom,
    type_: Atom,
    format: u8,
    bytes: &[u8],
) {
    let unit_bytes = usize::from(format / 8);
    assert_eq!(bytes.len() % unit_bytes, 0);
    conn.change_property(
        PropMode::REPLACE,
        window,
        property,
        type_,
        format,
        (bytes.len() / unit_bytes) as u32,
        bytes,
    )
    .unwrap()
    .check()
    .unwrap();
}

struct RawRead {
    type_: Atom,
    format: u8,
    bytes: Vec<u8>,
    incr: bool,
    chunks: usize,
}

fn raw_event(conn: &RustConnection, deadline: Instant, matches: impl Fn(&Event) -> bool) -> Event {
    loop {
        assert!(Instant::now() < deadline, "raw selection peer timed out");
        if let Some(event) = conn.poll_for_event().expect("polling raw peer") {
            if matches(&event) {
                return event;
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn raw_read(conn: &RustConnection, window: u32, target: Atom) -> RawRead {
    let selection = intern(conn, "CLIPBOARD");
    let property = intern(conn, "_BIFROST_TEST_PNG_READ");
    let deadline = Instant::now() + Duration::from_secs(10);
    conn.convert_selection(window, selection, target, property, 0u32)
        .unwrap()
        .check()
        .unwrap();
    conn.flush().unwrap();
    let Event::SelectionNotify(notify) = raw_event(conn, deadline, |event| {
        matches!(event, Event::SelectionNotify(notify)
            if notify.requestor == window && notify.selection == selection && notify.target == target)
    }) else {
        unreachable!("matched SelectionNotify");
    };
    assert_eq!(
        notify.property, property,
        "owner refused the requested target"
    );
    let read_property = || {
        let reply = conn
            .get_property(
                true,
                window,
                property,
                AtomEnum::ANY,
                0,
                (clipboard::MAX_IMAGE_BYTES / 4) as u32,
            )
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.bytes_after, 0, "property exceeded the PNG hard cap");
        reply
    };
    let initial = read_property();
    if initial.type_ != intern(conn, "INCR") {
        return RawRead {
            type_: initial.type_,
            format: initial.format,
            bytes: initial.value,
            incr: false,
            chunks: 0,
        };
    }
    assert_eq!(initial.format, 32);
    let announced = initial.value32().unwrap().next().expect("INCR byte count") as usize;
    assert!(announced <= clipboard::MAX_IMAGE_BYTES as usize);
    let mut bytes = Vec::with_capacity(announced);
    let mut chunks = 0;
    loop {
        conn.flush().unwrap();
        raw_event(conn, deadline, |event| {
            matches!(event, Event::PropertyNotify(event)
                if event.window == window && event.atom == property && event.state == Property::NEW_VALUE)
        });
        let chunk = read_property();
        assert_eq!(chunk.type_, target);
        assert_eq!(chunk.format, 8);
        if chunk.value.is_empty() {
            assert_eq!(bytes.len(), announced);
            return RawRead {
                type_: target,
                format: 8,
                bytes,
                incr: true,
                chunks,
            };
        }
        assert!(bytes.len() + chunk.value.len() <= clipboard::MAX_IMAGE_BYTES as usize);
        bytes.extend_from_slice(&chunk.value);
        chunks += 1;
    }
}

/// One valid RGBA pixel, with a large ancillary text chunk to force INCR without a codec.
fn large_png() -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
        png.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(bytes);
        let mut crc = u32::MAX;
        for byte in kind.iter().chain(bytes) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        png.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    let mut text = vec![b'x'; DEFAULT_MAX_REQUEST_SIZE + 64];
    text[..8].copy_from_slice(b"Comment\0");
    chunk(&mut png, b"tEXt", &text);
    // Zlib stored block: filter byte + RGBA(0x12,0x34,0x56,0xff), then Adler-32.
    chunk(
        &mut png,
        b"IDAT",
        &[
            0x78, 1, 1, 5, 0, 0xfa, 0xff, 0, 0x12, 0x34, 0x56, 0xff, 2, 0x94, 1, 0x9c,
        ],
    );
    chunk(&mut png, b"IEND", &[]);
    png
}

fn intern(conn: &RustConnection, name: &str) -> Atom {
    conn.intern_atom(false, name.as_bytes())
        .expect("interning a helper atom")
        .reply()
        .expect("the atom reply")
        .atom
}

/// A tiny raw-X11 CLIPBOARD owner: a second client that answers exactly the targets this
/// adapter asks for, including the KDE secret hint `xclip` cannot carry.
struct RawOwner {
    stop: Arc<AtomicBool>,
    png_requests: Arc<AtomicUsize>,
    incr_transfers: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl RawOwner {
    fn start(text: Option<&str>, png: Option<Vec<u8>>, hint: Option<&str>) -> Self {
        let (conn, screen_num) = x11rb::connect(None).expect("the helper display");
        let window = raw_window(&conn, screen_num);
        let atoms = [
            "CLIPBOARD",
            "TARGETS",
            "TIMESTAMP",
            "UTF8_STRING",
            "TEXT",
            "x-kde-passwordManagerHint",
            "image/png",
            "INCR",
        ]
        .map(|name| intern(&conn, name));
        conn.set_selection_owner(window, atoms[0], 0u32)
            .expect("the helper takes CLIPBOARD")
            .check()
            .expect("the helper owns CLIPBOARD");
        conn.flush().expect("flushing the helper connection");

        let stop = Arc::new(AtomicBool::new(false));
        let png_requests = Arc::new(AtomicUsize::new(0));
        let incr_transfers = Arc::new(AtomicUsize::new(0));
        let stop_for_thread = Arc::clone(&stop);
        let requests = Arc::clone(&png_requests);
        let transfers = Arc::clone(&incr_transfers);
        let text = text.map(str::to_owned);
        let png = png.map(Arc::new);
        let hint = hint.map(str::to_owned);
        let thread = thread::spawn(move || {
            let mut active: Option<RawIncr> = None;
            while !stop_for_thread.load(Ordering::Relaxed) {
                match conn.poll_for_event() {
                    Ok(Some(Event::SelectionRequest(request))) => {
                        if request.target == atoms[6] {
                            requests.fetch_add(1, Ordering::SeqCst);
                        }
                        if let Some(transfer) = serve(
                            &conn,
                            &atoms,
                            text.as_deref(),
                            png.as_ref(),
                            hint.as_deref(),
                            &request,
                        ) {
                            transfers.fetch_add(1, Ordering::SeqCst);
                            active = Some(transfer);
                        }
                    }
                    Ok(Some(Event::PropertyNotify(event))) if event.state == Property::DELETE => {
                        if let Some(transfer) = active.as_mut().filter(|transfer| {
                            transfer.window == event.window && transfer.property == event.atom
                        }) {
                            let end = (transfer.sent + DEFAULT_MAX_REQUEST_SIZE / 2)
                                .min(transfer.png.len());
                            let bytes = &transfer.png[transfer.sent..end];
                            raw_property(
                                &conn,
                                transfer.window,
                                transfer.property,
                                atoms[6],
                                8,
                                bytes,
                            );
                            transfer.sent = end;
                            if bytes.is_empty() {
                                active = None;
                            }
                            conn.flush().expect("flushing INCR chunk");
                        }
                    }
                    Ok(_) => thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            let _ = conn.set_selection_owner(AtomEnum::NONE, atoms[0], 0u32);
            let _ = conn.flush();
        });
        Self {
            stop,
            png_requests,
            incr_transfers,
            thread: Some(thread),
        }
    }
}

struct RawIncr {
    window: u32,
    property: Atom,
    png: Arc<Vec<u8>>,
    sent: usize,
}

impl Drop for RawOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answers metadata/text directly and PNG through independent ICCCM INCR framing.
fn serve(
    conn: &RustConnection,
    atoms: &[Atom; 8],
    text: Option<&str>,
    png: Option<&Arc<Vec<u8>>>,
    hint: Option<&str>,
    request: &SelectionRequestEvent,
) -> Option<RawIncr> {
    let property = if request.property == 0 {
        request.target
    } else {
        request.property
    };
    let mut metadata = Vec::new();
    let reply = if request.target == atoms[1] {
        let mut targets = vec![atoms[1], atoms[2]];
        if text.is_some() {
            targets.extend_from_slice(&atoms[3..5]);
        }
        if png.is_some() {
            targets.push(atoms[6]);
        }
        if hint.is_some() {
            targets.push(atoms[5]);
        }
        for atom in targets {
            metadata.extend_from_slice(&atom.to_ne_bytes());
        }
        Some((u32::from(AtomEnum::ATOM), 32, metadata.as_slice()))
    } else if request.target == atoms[2] {
        metadata.extend_from_slice(&0u32.to_ne_bytes());
        Some((u32::from(AtomEnum::INTEGER), 32, metadata.as_slice()))
    } else if request.target == atoms[3] || request.target == atoms[4] {
        text.map(|text| (request.target, 8, text.as_bytes()))
    } else if request.target == atoms[5] {
        hint.map(|hint| (request.target, 8, hint.as_bytes()))
    } else if request.target == atoms[6] {
        png.map(|png| (request.target, 8, png.as_slice()))
    } else {
        None
    };
    let mut active = None;
    let sent = if let Some((type_, format, bytes)) = reply {
        if request.target == atoms[6] && bytes.len() > DEFAULT_MAX_REQUEST_SIZE {
            conn.change_window_attributes(
                request.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
            raw_property(
                conn,
                request.requestor,
                property,
                atoms[7],
                32,
                &(bytes.len() as u32).to_ne_bytes(),
            );
            active = Some(RawIncr {
                window: request.requestor,
                property,
                png: Arc::clone(png.expect("PNG reply")),
                sent: 0,
            });
        } else {
            raw_property(conn, request.requestor, property, type_, format, bytes);
        }
        property
    } else {
        0
    };
    let event = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: atoms[0],
        target: request.target,
        property: sent,
    };
    conn.send_event(false, request.requestor, EventMask::NO_EVENT, event)
        .unwrap();
    conn.flush().unwrap();
    active
}
