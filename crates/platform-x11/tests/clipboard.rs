//! The real X11 CLIPBOARD, end to end, against a *different client*.
//!
//! Ignored by default, because it needs a real X display and `xclip`:
//!
//! ```text
//! xvfb-run -a cargo test -p platform-x11 --test clipboard -- --ignored --nocapture
//! ```
//!
//! The point is that nothing here is our own second connection pretending: the external
//! half is `xclip`, a separate process with its own window and selection owner, so a paste
//! that works really did cross the X server. `xclip` cannot carry KDE's
//! `x-kde-passwordManagerHint`, so the marker case drives a tiny in-test raw-X11 owner that
//! is still a *different* connection from the adapter under test.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clipboard::Marker;
use platform_x11::clipboard::{Clipboard, DEFAULT_MAX_REQUEST_SIZE};
use x11rb::connection::Connection as _;
use x11rb::protocol::bigreq::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
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
    clipboard.set(&out).expect("taking CLIPBOARD");
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
    assert!(snapshot.has_text, "xclip published text");
    assert_eq!(snapshot.text, incoming);
    assert!(snapshot.markers.is_empty(), "plain text carries no marker");
    println!("in: read {:?} after a change", snapshot.text);

    // 3. A real INCR transfer, both directions.
    //
    // This adapter streams above its own 64 KiB request size, and xclip streams above
    // `XExtendedMaxRequestSize / 4` (~1 MiB, ICCCM 2.5's "large" threshold). A payload
    // above both therefore cannot be a single property write in either direction, so a
    // byte-for-byte round trip is proof the INCR dance ran.
    let payload_len = 2 * 1024 * 1024;
    assert!(
        payload_len > DEFAULT_MAX_REQUEST_SIZE,
        "the payload must exceed this adapter's INCR threshold"
    );

    // xclip streams through INCR above `XExtendedMaxRequestSize / 4` (falling back to the
    // core maximum when the server has no BigRequests). Reading the same number and
    // asserting the payload clears it makes the "in" direction provably not a single
    // property write rather than merely large.
    let (probe, _) = x11rb::connect(None).expect("probe display");
    let max_units = probe
        .bigreq_enable()
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.maximum_request_length)
        .filter(|max| *max > 0)
        .unwrap_or_else(|| u32::from(probe.setup().maximum_request_length));
    let xclip_chunk = (max_units / 4) as usize;
    assert!(
        payload_len > xclip_chunk,
        "the payload must exceed xclip's {xclip_chunk}-byte INCR threshold"
    );
    println!("xclip INCR threshold: {xclip_chunk} bytes (max request {max_units} 4-byte units)");

    let big: Vec<u8> = (0..payload_len).map(|i| b'a' + (i % 26) as u8).collect();
    let big_text = std::str::from_utf8(&big).expect("the payload is ASCII");

    clipboard
        .set(big_text)
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
    assert!(snapshot.has_text);
    assert_eq!(
        snapshot.text.len(),
        payload_len,
        "INCR in: read {} of {payload_len} bytes",
        snapshot.text.len()
    );
    assert_eq!(
        snapshot.text.as_bytes(),
        &big[..],
        "INCR in is byte-for-byte"
    );
    println!(
        "INCR in: {} bytes read from xclip's selection",
        snapshot.text.len()
    );

    // 4. KDE's secret hint, and its absence.
    let secret = "lws-clipboard-kde-secret-item";
    let owner = RawOwner::start(secret, Some("secret"));
    assert!(
        wait_for_change(&clipboard),
        "the helper taking CLIPBOARD was observed"
    );
    let snapshot = clipboard.read().expect("reading the helper's selection");
    assert!(snapshot.has_text);
    assert_eq!(snapshot.text, secret);
    assert_eq!(
        snapshot.markers,
        vec![Marker::KdeSecret],
        "x-kde-passwordManagerHint=secret is classified as a secret"
    );
    println!(
        "kde hint: read {:?} with {:?}",
        snapshot.text, snapshot.markers
    );
    drop(owner);

    drain(&clipboard);
    let plain = "lws-clipboard-plain-item";
    xclip_in(plain.as_bytes());
    assert!(wait_for_change(&clipboard), "the plain write was observed");
    let snapshot = clipboard.read().expect("reading the plain selection");
    assert!(snapshot.has_text);
    assert_eq!(snapshot.text, plain);
    assert!(
        snapshot.markers.is_empty(),
        "an item with no secret hint carries no marker"
    );
    println!("plain: read {:?} with no marker", snapshot.text);
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
    thread: Option<JoinHandle<()>>,
}

impl RawOwner {
    fn start(text: &str, hint: Option<&str>) -> Self {
        let (conn, screen_num) = x11rb::connect(None).expect("the helper display");
        let root = conn.setup().roots[screen_num].root;
        let window = conn.generate_id().expect("a helper window id");
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .expect("the helper window");

        let atoms = [
            "CLIPBOARD",
            "TARGETS",
            "TIMESTAMP",
            "UTF8_STRING",
            "TEXT",
            "x-kde-passwordManagerHint",
        ]
        .map(|name| intern(&conn, name));

        conn.set_selection_owner(window, atoms[0], 0u32)
            .expect("the helper takes CLIPBOARD")
            .check()
            .expect("the helper owns CLIPBOARD");
        conn.flush().expect("flushing the helper connection");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let text = text.to_owned();
        let hint = hint.map(str::to_owned);
        let thread = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                match conn.poll_for_event() {
                    Ok(Some(Event::SelectionRequest(request))) => {
                        serve(&conn, &atoms, &text, hint.as_deref(), &request);
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
            thread: Some(thread),
        }
    }
}

impl Drop for RawOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answers one `SelectionRequest` the way a selection owner must.
fn serve(
    conn: &RustConnection,
    atoms: &[Atom; 6],
    text: &str,
    hint: Option<&str>,
    request: &SelectionRequestEvent,
) {
    let clipboard = atoms[0];
    let property = if request.property == 0 {
        request.target
    } else {
        request.property
    };

    let reply = if request.target == atoms[1] {
        let mut data = Vec::new();
        for atom in atoms {
            data.extend_from_slice(&atom.to_ne_bytes());
        }
        Some((u32::from(AtomEnum::ATOM), 32u8, data))
    } else if request.target == atoms[2] {
        Some((
            u32::from(AtomEnum::INTEGER),
            32u8,
            0u32.to_ne_bytes().to_vec(),
        ))
    } else if request.target == atoms[3] || request.target == atoms[4] {
        Some((request.target, 8u8, text.as_bytes().to_vec()))
    } else if request.target == atoms[5] {
        hint.map(|hint| (atoms[5], 8u8, hint.as_bytes().to_vec()))
    } else {
        None
    };

    let sent = match reply {
        Some((type_, format, data)) => {
            let elements = match format {
                32 => data.len() / 4,
                _ => data.len(),
            };
            let _ = conn.change_property(
                PropMode::REPLACE,
                request.requestor,
                property,
                type_,
                format,
                elements as u32,
                &data,
            );
            property
        }
        None => 0,
    };

    let event = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: clipboard,
        target: request.target,
        property: sent,
    };
    let _ = conn.send_event(false, request.requestor, EventMask::NO_EVENT, event);
    let _ = conn.flush();
}
