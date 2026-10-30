//! The real Windows clipboard, end to end: a change the harness makes is seen, and an item
//! a password manager would mark is classified as a secret.
//!
//! Ignored by default, because it overwrites the clipboard of whoever runs it (and CI
//! runs on machines nobody should be typing on):
//!
//! ```text
//! cargo test -p platform-win --test clipboard -- --ignored --nocapture
//! ```
//!
//! The plain case uses PowerShell's own `Set-Clipboard` — an external writer, which is the
//! point: the adapter must see a change it did not make. The secret case registers
//! `ExcludeClipboardContentFromMonitorProcessing` and puts a `DWORD 1` under it, the way a
//! password manager does; `SetClipboardData` replaces that one format and leaves the text
//! alone, so no marshalling of the text is needed.

#![cfg(windows)]

use std::process::Command;
use std::time::{Duration, Instant};

use clipboard::{ClipboardContent, Marker};
use platform_win::clipboard::{read_clipboard, write_clipboard, Clipboard};

/// Runs PowerShell with one script, or panics with its output.
fn powershell(script: &str) {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell runs");
    assert!(
        output.status.success(),
        "powershell failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Waits until the clipboard's sequence number moves past `previous`.
fn wait_for_change(listener: &Clipboard, previous: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let sequence = listener.sequence();
        if sequence != previous {
            return sequence;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the clipboard did not change within 10 s");
}

#[test]
#[ignore = "overwrites the clipboard of whoever runs it"]
fn a_plain_change_is_seen_and_a_marked_item_is_a_secret() {
    let listener = Clipboard::listen().expect("the listener window");
    let marker = format!("bifrost-clipboard-smoke-{}", std::process::id());

    // 1. An external writer changes the clipboard; the adapter must see it and read it.
    let before = listener.sequence();
    powershell(&format!("Set-Clipboard -Value '{marker}'"));
    let after = wait_for_change(&listener, before);
    assert!(after != before);
    let snapshot = read_clipboard().expect("reading the clipboard");
    assert_eq!(snapshot.content, Some(ClipboardContent::Text(marker)));
    assert!(
        snapshot.markers.is_empty(),
        "plain text must carry no marker: {:?}",
        snapshot.markers
    );
    println!("plain: read {:?} at sequence {after}", snapshot.content);

    // 2. A password-manager-shaped item: the same text plus
    //    `ExcludeClipboardContentFromMonitorProcessing`. (The listener must report the
    //    change as well, which is what the notification is for.)
    let before = listener.sequence();
    let script = r#"
Add-Type -Namespace Lws -Name Clip -MemberDefinition @'
[DllImport("user32.dll", SetLastError=true)] public static extern bool OpenClipboard(IntPtr hWndNewOwner);
[DllImport("user32.dll", SetLastError=true)] public static extern bool CloseClipboard();
[DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SetClipboardData(uint uFormat, IntPtr hMem);
[DllImport("user32.dll", SetLastError=true)] public static extern uint RegisterClipboardFormat(string lpszFormat);
[DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr GlobalAlloc(uint uFlags, UIntPtr dwBytes);
[DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr GlobalLock(IntPtr hMem);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GlobalUnlock(IntPtr hMem);
'@
$format = [Lws.Clip]::RegisterClipboardFormat("ExcludeClipboardContentFromMonitorProcessing")
if ($format -eq 0) { throw "RegisterClipboardFormat failed" }
$handle = [Lws.Clip]::GlobalAlloc(0x0042, [UIntPtr]::new(4))   # GHND: moveable, zeroed
if ($handle -eq [IntPtr]::Zero) { throw "GlobalAlloc failed" }
$pointer = [Lws.Clip]::GlobalLock($handle)
[System.Runtime.InteropServices.Marshal]::WriteInt32($pointer, 1)   # "do monitor it" flag
[void][Lws.Clip]::GlobalUnlock($handle)
if (-not [Lws.Clip]::OpenClipboard([IntPtr]::Zero)) { throw "OpenClipboard failed" }
try {
  if ([Lws.Clip]::SetClipboardData($format, $handle) -eq [IntPtr]::Zero) { throw "SetClipboardData failed" }
} finally { [void][Lws.Clip]::CloseClipboard() }
"#;
    powershell(script);
    // The format was set on the same item, so the sequence number may not move; the
    // listener's notification is the other signal, and the read is what matters here.
    let _ = wait_for_change(&listener, before);
    let snapshot = read_clipboard().expect("reading the marked clipboard");
    assert_eq!(
        snapshot.markers,
        vec![Marker::ExcludeFromMonitor],
        "the marked format must classify as a secret (text {:?})",
        snapshot.content
    );
    println!(
        "marked: markers {:?} on {:?}",
        snapshot.markers, snapshot.content
    );

    // 3. Clean up: leave plain text behind, never the marked item.
    powershell("Set-Clipboard -Value 'bifrost-clipboard-smoke done'");
}

/// The writer half of the adapter: text this process puts on the clipboard is exactly what
/// the next reader sees, the change counter moves (which is the only signal the agent's echo
/// suppression has on Windows), and clearing leaves no text at all.
///
/// Ignored for the same reason as the test above: it is the clipboard of whoever runs it.
#[test]
#[ignore = "overwrites the clipboard of whoever runs it"]
fn a_written_text_round_trips_and_clearing_removes_it() {
    let listener = Clipboard::listen().expect("the listener window");

    // Non-ASCII and non-BMP, so the UTF-16 encoding is exercised for real (a Cyrillic
    // letter is one code unit, the crab is a surrogate pair).
    let text = format!(
        "bifrost-clipboard-write-{}\u{4F60}\u{597D}\u{1F980}",
        std::process::id()
    );
    let before = listener.sequence();
    write_clipboard(Some(&text)).expect("the text is put on the clipboard");
    let after = listener.sequence();
    assert!(
        after != before,
        "SetClipboardData moves the change counter: {after} == {before}"
    );

    let snapshot = read_clipboard().expect("reading what was written");
    assert_eq!(
        snapshot.content,
        Some(ClipboardContent::Text(text)),
        "the text round-trips byte for byte"
    );
    assert!(snapshot.markers.is_empty(), "plain text carries no marker");
    println!("set: read {:?} back at sequence {after}", snapshot.content);

    // And clearing removes it: no text, and the counter moved again.
    write_clipboard(None).expect("the clipboard is emptied");
    let snapshot = read_clipboard().expect("reading the emptied clipboard");
    assert!(snapshot.content.is_none(), "nothing to paste after clear()");
    assert!(
        listener.sequence() != after,
        "EmptyClipboard moves the change counter too"
    );
}
