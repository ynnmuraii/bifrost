//! Embeds a `PerMonitorV2` DPI-awareness manifest into the example (and binary)
//! executables.
//!
//! Why a manifest and not `SetProcessDpiAwarenessContext`: KVM-010/KVM-011
//! require the *executable* to be PerMonitorV2-aware before any window or
//! `GetCursorPos`/`EnumDisplayMonitors` call, otherwise Windows virtualises the
//! coordinates at 150/200% scaling and edge transitions (AC-08) cannot work. A
//! manifest is honoured from process start; an API call in `main` would work for
//! the examples too, but it is not what the shipped `lws-agent`/`lws-gui` will
//! use, so the spike exercises the same mechanism the product needs. Cargo build
//! scripts can attach link arguments per target kind, so this works for
//! `cargo build --examples` without any extra crate.
//!
//! On non-Windows *targets* nothing is emitted, so a Linux `cargo check` of this
//! package stays clean.

use std::env;
use std::fs;
use std::path::PathBuf;

/// 2016-era `dpiAwareness` wins on Windows 10+; the 2005 `dpiAware` element is
/// the pre-1607 fallback and must be `true/pm` (per-monitor, v1).
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // `/MANIFEST:EMBED` and `/MANIFESTINPUT:` are MSVC `link.exe` switches. The
    // shipped `lws-agent`/`lws-gui` are MSVC builds, which is what KVM-010/KVM-011
    // require; a mingw/gnullvm link cannot take these flags, so there the examples
    // fall back to `dpi::ensure_per_monitor_v2()` at startup instead (see the
    // crate docs and `src/dpi.rs`).
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_path = out_dir.join("lws-per-monitor-v2.manifest");
    fs::write(&manifest_path, MANIFEST).expect("write manifest");

    let manifest_input = format!("/MANIFESTINPUT:{}", manifest_path.display());

    // `rustc-link-arg-examples` is per target kind; this package has examples,
    // not binaries, so only the example link step gets the manifest. If a binary
    // target is ever added here, add the matching `-bins` arguments.
    println!("cargo::rustc-link-arg-examples=/MANIFEST:EMBED");
    println!("cargo::rustc-link-arg-examples={manifest_input}");
}
