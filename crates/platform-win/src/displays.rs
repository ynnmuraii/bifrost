//! Monitor enumeration for `DisplaysReport` (KVM-040).
//!
//! `docs/03-architecture.md`, «Координаты»: «Logical unit: на Windows — DIP (1/96
//! дюйма при текущем DPI монитора, Per-Monitor-V2)», and the node logical space has its
//! origin at the top-left corner of the primary display, so the logical bounds of a
//! monitor are its virtual-screen rectangle divided by that monitor's scale
//! (`dpi / 96`). `native_*` keeps the physical pixels `EnumDisplayMonitors` reports.
//!
//! Identity: the monitor's device name (`\\.\DISPLAY1`) — stable for the session and
//! distinct per monitor, which is what `Display.id` needs («stable within node»).
//!
//! Not implemented here: per-display rotation. Windows rotates the *logical* desktop
//! itself, so the logical bounds are already correct; only the diagnostic `rotation`
//! field would need `EnumDisplaySettingsW`/`DEVMODEW`, and nothing in `geometry` uses
//! it.
#![cfg(windows)]

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

/// One monitor, in both coordinate spaces.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub id: String,
    /// Logical bounds (DIP): `[x, y, width, height]`.
    pub logical: [f64; 4],
    /// Physical bounds in device pixels: `[x, y, width, height]`.
    pub native: [i32; 4],
    /// Effective DPI divided by 96.
    pub scale: f64,
    /// Whether this is the primary monitor (the node logical origin).
    pub primary: bool,
}

/// Monitor enumeration failed before producing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaysError(pub String);

impl std::fmt::Display for DisplaysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot enumerate monitors: {}", self.0)
    }
}

impl std::error::Error for DisplaysError {}

/// Every monitor of the virtual desktop, primary first.
///
/// Must be called on a thread with a message queue (the agent's main thread is one):
/// `EnumDisplayMonitors` enumerates the *calling thread's* desktop.
pub fn enumerate() -> Result<Vec<Monitor>, DisplaysError> {
    // Physical bounds are only physical in a Per-Monitor-V2 process; otherwise the
    // virtual desktop is virtualised to the system DPI and the logical conversion
    // below would be wrong. Idempotent, and it must happen before any window exists
    // (the agent has none).
    let _ = crate::dpi::ensure_per_monitor_v2();

    let mut collected: Vec<Monitor> = Vec::new();
    let pointer = &mut collected as *mut Vec<Monitor> as isize;
    // SAFETY: the callback is a plain function that only touches the `Vec` we pass
    // through `dwdata`; it stays alive for the duration of the call.
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM(pointer)) };
    if !ok.as_bool() {
        return Err(DisplaysError(
            "EnumDisplayMonitors returned false".to_owned(),
        ));
    }
    if collected.is_empty() {
        return Err(DisplaysError("the desktop reports no monitors".to_owned()));
    }
    // Primary first: it hosts the node logical origin, and a stable order keeps the
    // reported set comparable between calls.
    collected.sort_by(|left, right| {
        right
            .primary
            .cmp(&left.primary)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(collected)
}

/// The `MONITORENUMPROC` body: one monitor per call.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `*mut Vec<Monitor>` passed by `enumerate`, still alive.
    let collected = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a correctly sized MONITORINFOEXW (cbSize set), which is why
    // the W API accepts it as a MONITORINFO pointer.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) };
    if !ok.as_bool() {
        // Keep enumerating: one unreadable monitor is not a reason to lose the rest.
        return TRUE;
    }

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: the monitor handle came from the enumeration; the two out-parameters are
    // valid for the call. A failure leaves the 96 DPI default, i.e. scale 1.0.
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale = if dpi_x >= 96 {
        f64::from(dpi_x) / 96.0
    } else {
        1.0
    };
    if scale <= 0.0 || !scale.is_finite() {
        return TRUE;
    }

    let native = [
        info.monitorInfo.rcMonitor.left,
        info.monitorInfo.rcMonitor.top,
        info.monitorInfo.rcMonitor.right - info.monitorInfo.rcMonitor.left,
        info.monitorInfo.rcMonitor.bottom - info.monitorInfo.rcMonitor.top,
    ];
    let logical = [
        f64::from(native[0]) / scale,
        f64::from(native[1]) / scale,
        f64::from(native[2]) / scale,
        f64::from(native[3]) / scale,
    ];

    collected.push(Monitor {
        id: device_name(&info.szDevice),
        logical,
        native,
        scale,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    });
    TRUE
}

/// `MONITORINFOEXW.szDevice` is a fixed-size UTF-16 buffer.
fn device_name(buffer: &[u16; 32]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the cross-check test reads the desktop metrics.
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};

    #[test]
    fn a_device_name_is_read_up_to_its_nul() {
        let mut buffer = [0u16; 32];
        for (index, unit) in "\\\\.\\DISPLAY1".encode_utf16().enumerate() {
            buffer[index] = unit;
        }
        assert_eq!(device_name(&buffer), "\\\\.\\DISPLAY1");

        let empty = [0u16; 32];
        assert_eq!(device_name(&empty), "");
    }

    /// The invariant that catches a monitor being silently dropped: the number of
    /// callbacks `EnumDisplayMonitors` makes must equal the number of monitors we
    /// describe (a failing `GetMonitorInfoW`, a wrong `cbSize` or an early `FALSE`
    /// would all show up here), and the desktop must have at least one.
    ///
    /// Deliberately no assertion on *how many* monitors this host has: the count depends
    /// on what is switched on at the moment, which is exactly what the KVM-001 inventory
    /// has to be read against.
    #[test]
    fn nothing_the_os_reports_is_dropped() {
        let mut raw = 0usize;
        let pointer = &mut raw as *mut usize as isize;
        // SAFETY: the callback only increments the counter behind `data`, which outlives
        // the call.
        let ok = unsafe { EnumDisplayMonitors(None, None, Some(count_monitor), LPARAM(pointer)) };
        assert!(ok.as_bool(), "EnumDisplayMonitors returned false");

        let monitors = enumerate().expect("a Windows host always has a monitor");
        assert_eq!(
            raw,
            monitors.len(),
            "the OS offered {raw} monitors and {} were described",
            monitors.len()
        );
        assert!(raw >= 1, "a desktop always has at least one monitor");
        // The OS agrees with itself; SM_CMONITORS counts the monitors of the desktop
        // (and may include a mirroring pseudo-device, so it is reported, not asserted).
        // SAFETY: no preconditions.
        let system_metric = unsafe { GetSystemMetrics(SM_CMONITORS) };
        assert!(system_metric >= 1);
        println!(
            "monitors: raw={raw} described={} SM_CMONITORS={system_metric}",
            monitors.len()
        );
    }

    /// Counts one `MONITORENUMPROC` call without describing anything.
    unsafe extern "system" fn count_monitor(
        _monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the `*mut usize` the test passed and keeps alive.
        let count = unsafe { &mut *(data.0 as *mut usize) };
        *count += 1;
        TRUE
    }

    #[test]
    fn the_primary_monitor_is_reported_first_and_scales_are_usable() {
        // The real desktop of the test host: one monitor at least, ids lowercase-hex-free
        // but non-empty, and every scale strictly positive (geometry rejects the rest).
        let monitors = enumerate().expect("a Windows host always has a monitor");
        println!("monitors: {monitors:#?}");
        assert!(!monitors.is_empty());
        assert!(
            monitors[0].primary,
            "the primary monitor is sorted first: {monitors:?}"
        );
        assert!(monitors.iter().filter(|monitor| monitor.primary).count() <= 1);
        for monitor in &monitors {
            assert!(!monitor.id.is_empty());
            assert!(monitor.scale.is_finite() && monitor.scale > 0.0);
            assert!(monitor.logical[2] > 0.0 && monitor.logical[3] > 0.0);
            assert!(monitor.native[2] > 0 && monitor.native[3] > 0);
            // The logical rectangle is the physical one divided by the scale.
            assert!(
                (monitor.logical[2] - f64::from(monitor.native[2]) / monitor.scale).abs() < 1e-9
            );
        }
    }
}
