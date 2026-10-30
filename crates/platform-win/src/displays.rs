//! Monitor enumeration for `DisplaysReport` (KVM-040).
//!
//! `docs/03-architecture.md`, «Координаты»: «Logical unit: на Windows — DIP (1/96
//! дюйма при текущем DPI монитора, Per-Monitor-V2)», and the node logical space has its
//! origin at the top-left corner of the primary display, so the logical bounds of a
//! monitor are its virtual-screen rectangle divided by that monitor's scale
//! (`dpi / 96`). `native_*` keeps the physical pixels `EnumDisplayMonitors` reports.
//!
//! Identity: the monitor's device-interface path (`\\?\DISPLAY#<mfg+product>#<instance>
//! #{guid}`) read with `EnumDisplayDevicesW(EDD_GET_DEVICE_INTERFACE_NAME)`, which is derived
//! from the monitor's EDID and its instance rather than from the adapter numbering, so it
//! survives the renumbering of `\\.\DISPLAYn` when monitors are added or removed. The device
//! name is kept beside it: the log prints it, and core's alias maps a layout written under
//! the old name to the display that now carries that name. When Windows will not describe an
//! interface (the call fails, or the output has no monitor attached), the device name is the
//! id — the pre-existing behaviour.
//!
//! Current-mode orientation is used only to orient EDID millimeters. Windows already
//! rotates desktop coordinates; physical metadata never rescales or rotates those.
#![cfg(windows)]

use windows::core::{w, BOOL, GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV,
    SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW,
    DISPLAY_DEVICEW, DMDO_180, DMDO_270, DMDO_90, DMDO_DEFAULT, DM_DISPLAYORIENTATION,
    ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ, REG_BINARY};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY,
};

/// One monitor, in both coordinate spaces.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// Stable id: the monitor's device-interface path (`\\?\DISPLAY#…`) when Windows
    /// describes one, the `\\.\DISPLAYn` device name otherwise.
    pub id: String,
    /// The adapter device name (`\\.\DISPLAYn`) this monitor answers at, kept for the log
    /// and for the alias that maps a document written before the id changed.
    pub device_name: String,
    /// Logical bounds (DIP): `[x, y, width, height]`.
    pub logical: [f64; 4],
    /// Physical bounds in device pixels: `[x, y, width, height]`.
    pub native: [i32; 4],
    /// Effective DPI divided by 96.
    pub scale: f64,
    /// Whether this is the primary monitor (the node logical origin).
    pub primary: bool,
    pub physical_size: Option<[f64; 2]>,
    pub physical_size_source: String,
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
    // Registry queries stay outside the GDI enumeration callback.
    enrich_physical_sizes(&mut collected);
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

    let device = utf16_z(&info.szDevice);
    let id = stable_id(device_interface_name(&device).as_deref(), &device);

    collected.push(Monitor {
        id,
        device_name: device,
        logical,
        native,
        scale,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        physical_size: None,
        physical_size_source: String::new(),
    });
    TRUE
}

/// The `id` a monitor is reported under.
///
/// The device-interface name wins when Windows gives one: it is built from the monitor's
/// EDID and its instance and does not move with the adapter's `\\.\DISPLAYn` numbering. An
/// empty or missing interface leaves the device name, which is what this crate reported
/// before the interface name was read.
fn stable_id(interface: Option<&str>, device_name: &str) -> String {
    match interface {
        Some(interface) if !interface.is_empty() => interface.to_owned(),
        _ => device_name.to_owned(),
    }
}

/// The monitor's device-interface path, or `None` when Windows will not describe one.
///
/// `EnumDisplayDevicesW` on the adapter device name with `EDD_GET_DEVICE_INTERFACE_NAME`
/// answers with the monitor's interface name (`\\?\DISPLAY#DEL4098#5&…#{e6f07b5f-…}`), whose
/// instance component keeps two identical models apart. An adapter output with no monitor
/// attached (a spare port) answers with an empty `DeviceID`, which is also "no interface".
fn device_interface_name(device: &str) -> Option<String> {
    let name: Vec<u16> = device.encode_utf16().chain(core::iter::once(0)).collect();
    let mut monitor = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: `name` is NUL-terminated UTF-16 and outlives the call, `monitor.cb` is the
    // size the API checks, and `monitor` is valid for writes for the duration.
    let ok = unsafe {
        EnumDisplayDevicesW(
            PCWSTR(name.as_ptr()),
            0,
            &mut monitor,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let interface = utf16_z(&monitor.DeviceID);
    (!interface.is_empty()).then_some(interface)
}

/// One fixed-size UTF-16 buffer as text, up to its NUL (or its end).
fn utf16_z(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Device interfaces encode the complete PnP instance, not just the model.
fn monitor_instance(interface: &str) -> Option<String> {
    let mut parts = interface.strip_prefix("\\\\?\\")?.split('#');
    let class = parts.next()?;
    let model = parts.next()?;
    let instance = parts.next()?;
    let guid = parts.next()?;
    if !class.eq_ignore_ascii_case("DISPLAY")
        || model.is_empty()
        || instance.is_empty()
        || !guid.starts_with('{')
        || parts.next().is_some()
    {
        return None;
    }
    Some(format!("DISPLAY\\{model}\\{instance}"))
}

fn oriented_size(size: [f64; 2], rotated: bool) -> [f64; 2] {
    if rotated {
        [size[1], size[0]]
    } else {
        size
    }
}

fn monitor_rotated(device: &str) -> Option<bool> {
    let name: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: the terminated name and correctly sized output live throughout the call.
    if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }
        .as_bool()
    {
        return None;
    }
    if !mode.dmFields.contains(DM_DISPLAYORIENTATION) {
        return None;
    }
    // SAFETY: EnumDisplaySettingsW fills the display (not printer) union member.
    let orientation = unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation };
    if orientation == DMDO_90 || orientation == DMDO_270 {
        Some(true)
    } else if orientation == DMDO_DEFAULT || orientation == DMDO_180 {
        Some(false)
    } else {
        None
    }
}

fn enrich_physical_sizes(monitors: &mut [Monitor]) {
    let monitor_class = GUID::from_u128(0x4d36e96e_e325_11ce_bfc1_08002be10318);
    // SAFETY: class GUID is valid; no parent window or enumerator is required.
    let Ok(devices) = (unsafe {
        SetupDiGetClassDevsW(Some(&monitor_class), PCWSTR::null(), None, DIGCF_PRESENT)
    }) else {
        return;
    };
    let mut index = 0;
    let mut edid = [0u8; 32768];
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: the device set is live and data has the API-required size.
        if unsafe { SetupDiEnumDeviceInfo(devices, index, &mut data) }.is_err() {
            break;
        }
        index += 1;
        let mut instance = [0u16; 512];
        // SAFETY: writable instance buffer, valid device record from this set.
        if unsafe { SetupDiGetDeviceInstanceIdW(devices, &data, Some(&mut instance), None) }
            .is_err()
        {
            continue;
        }
        let instance = utf16_z(&instance);
        let Some(monitor) = monitors.iter_mut().find(|monitor| {
            monitor_instance(&monitor.id).is_some_and(|id| id.eq_ignore_ascii_case(&instance))
        }) else {
            continue;
        };
        // SAFETY: opens only this exact monitor's device registry key, read-only.
        let Ok(key) = (unsafe {
            SetupDiOpenDevRegKey(devices, &data, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0)
        }) else {
            continue;
        };
        let mut kind = REG_BINARY;
        let mut len = edid.len() as u32;
        // SAFETY: the buffer is len bytes and all output pointers remain valid.
        let status = unsafe {
            RegQueryValueExW(
                key,
                w!("EDID"),
                None,
                Some(&mut kind),
                Some(edid.as_mut_ptr()),
                Some(&mut len),
            )
        };
        // SAFETY: key was opened above and is no longer used.
        let _ = unsafe { RegCloseKey(key) };
        if status.is_err() || kind != REG_BINARY || len as usize > edid.len() {
            continue;
        }
        if let (Some(size), Some(rotated)) = (
            parse_edid_size(&edid[..len as usize]),
            monitor_rotated(&monitor.device_name),
        ) {
            monitor.physical_size = Some(oriented_size(size, rotated));
            monitor.physical_size_source = "edid".to_owned();
        }
    }
    // SAFETY: releases the single device set created by this function.
    let _ = unsafe { SetupDiDestroyDeviceInfoList(devices) };
}

/// EDID base block and every advertised extension must be complete and checksummed.
fn parse_edid_size(edid: &[u8]) -> Option<[f64; 2]> {
    if edid.len() < 128
        || edid[..8] != [0, 255, 255, 255, 255, 255, 255, 0]
        || edid.len() != (usize::from(edid[126]) + 1) * 128
        || edid
            .as_chunks::<128>()
            .0
            .iter()
            .any(|block| block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0)
    {
        return None;
    }
    for timing in edid[54..126].as_chunks::<18>().0 {
        if timing[0] == 0 && timing[1] == 0 {
            continue;
        }
        let width = u16::from(timing[12]) | (u16::from(timing[14] & 0xf0) << 4);
        let height = u16::from(timing[13]) | (u16::from(timing[14] & 0x0f) << 8);
        if width > 0 && height > 0 {
            return Some([f64::from(width), f64::from(height)]);
        }
    }
    // EDID 1.4 uses a zero axis for aspect-only encoding, not centimeters.
    (edid[21] > 0 && edid[22] > 0)
        .then_some([f64::from(edid[21]) * 10.0, f64::from(edid[22]) * 10.0])
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the cross-check test reads the desktop metrics.
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};

    fn checksum(edid: &mut [u8]) {
        for block in edid.as_chunks_mut::<128>().0 {
            block[127] = 0;
            block[127] =
                0u8.wrapping_sub(block[..127].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)));
        }
    }

    fn base_edid() -> [u8; 128] {
        let mut edid = [0u8; 128];
        edid[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
        edid[21] = 53;
        edid[22] = 30;
        checksum(&mut edid);
        edid
    }

    #[test]
    fn edid_prefers_detailed_mm_then_base_cm_and_rotates() {
        let mut edid = base_edid();
        assert_eq!(parse_edid_size(&edid), Some([530.0, 300.0]));
        edid[54] = 1;
        edid[66] = 0x58;
        edid[67] = 0x54;
        edid[68] = 0x21;
        checksum(&mut edid);
        assert_eq!(parse_edid_size(&edid), Some([600.0, 340.0]));
        assert_eq!(
            oriented_size(parse_edid_size(&edid).unwrap(), true),
            [340.0, 600.0]
        );
        edid[54] = 0;
        checksum(&mut edid);
        assert_eq!(parse_edid_size(&edid), Some([530.0, 300.0]));
        edid[21] = 0;
        checksum(&mut edid);
        assert_eq!(parse_edid_size(&edid), None);
    }

    #[test]
    fn malformed_edid_is_unknown() {
        let edid = base_edid();
        assert!(parse_edid_size(&edid[..127]).is_none());
        let mut bad = edid;
        bad[0] = 1;
        checksum(&mut bad);
        assert!(parse_edid_size(&bad).is_none());
        bad = edid;
        bad[20] ^= 1;
        assert!(parse_edid_size(&bad).is_none());
        bad = edid;
        bad[126] = 1;
        checksum(&mut bad);
        assert!(parse_edid_size(&bad).is_none());
        let mut extended = [0u8; 256];
        extended[..128].copy_from_slice(&bad);
        checksum(&mut extended);
        assert_eq!(parse_edid_size(&extended), Some([530.0, 300.0]));
        extended[130] = 1;
        assert!(parse_edid_size(&extended).is_none());
    }

    #[test]
    fn instance_matching_never_confuses_identical_models() {
        let first = monitor_instance("\\\\?\\DISPLAY#DEL4098#instance-a#{guid}").unwrap();
        let second = monitor_instance("\\\\?\\DISPLAY#DEL4098#instance-b#{guid}").unwrap();
        assert!(first.eq_ignore_ascii_case("display\\del4098\\INSTANCE-A"));
        assert!(!first.eq_ignore_ascii_case(&second));
        assert!(monitor_instance("\\\\.\\DISPLAY1").is_none());
        assert!(monitor_instance("\\\\?\\DISPLAY#DEL4098##{guid}").is_none());
    }

    #[test]
    fn a_device_name_is_read_up_to_its_nul() {
        let mut buffer = [0u16; 32];
        for (index, unit) in "\\\\.\\DISPLAY1".encode_utf16().enumerate() {
            buffer[index] = unit;
        }
        assert_eq!(utf16_z(&buffer), "\\\\.\\DISPLAY1");

        let empty = [0u16; 32];
        assert_eq!(utf16_z(&empty), "");
    }

    #[test]
    fn a_monitor_without_an_interface_keeps_its_device_name() {
        // `EnumDisplayDevicesW` failed, or the output has no monitor attached: the id is the
        // device name, exactly what this crate reported before.
        assert_eq!(stable_id(None, "\\\\.\\DISPLAY2"), "\\\\.\\DISPLAY2");
        assert_eq!(stable_id(Some(""), "\\\\.\\DISPLAY2"), "\\\\.\\DISPLAY2");
        let interface =
            "\\\\?\\DISPLAY#DEL4098#5&2a4b1c3&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(stable_id(Some(interface), "\\\\.\\DISPLAY1"), interface);
    }

    #[test]
    fn two_identical_monitors_get_different_ids() {
        // Same model, same EDID-derived prefix, different instance component: the interface
        // name is what keeps the two apart, which the hardware id alone would not.
        let left =
            "\\\\?\\DISPLAY#DEL4098#5&2a4b1c3&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        let right =
            "\\\\?\\DISPLAY#DEL4098#5&2a4b1c3&0&UID4354#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(stable_id(Some(left), "\\\\.\\DISPLAY1"), left);
        assert_eq!(stable_id(Some(right), "\\\\.\\DISPLAY2"), right);
        assert_ne!(
            stable_id(Some(left), "\\\\.\\DISPLAY1"),
            stable_id(Some(right), "\\\\.\\DISPLAY2")
        );
    }

    /// The id `enumerate` reports is the interface name when the enumeration can read one,
    /// and the device name otherwise; both cases must still be distinct per monitor.
    #[test]
    fn enumerated_ids_are_distinct_and_carry_their_device_name() {
        let monitors = enumerate().expect("a Windows host always has a monitor");
        for monitor in &monitors {
            assert!(!monitor.id.is_empty());
            assert!(
                monitor.device_name.starts_with("\\\\.\\DISPLAY"),
                "device name: {}",
                monitor.device_name
            );
        }
        let ids: Vec<&str> = monitors.iter().map(|monitor| monitor.id.as_str()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids: {ids:?}");
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

    /// The agent answers `QueryDisplays` from a `spawn_blocking` thread: the
    /// enumeration must describe the same desktop from a thread with no message queue
    /// of its own. `EnumDisplayMonitors` enumerates the calling thread's desktop, and
    /// every thread of this process shares one — this test is what proves moving the
    /// call off the runtime thread changes nothing.
    #[test]
    fn a_thread_without_a_message_queue_sees_the_same_monitors() {
        let here = enumerate().expect("a Windows host always has a monitor");
        let there =
            std::thread::spawn(|| enumerate().expect("a Windows host always has a monitor"))
                .join()
                .expect("the thread ran");
        let ids = |monitors: &[Monitor]| -> Vec<String> {
            monitors.iter().map(|monitor| monitor.id.clone()).collect()
        };
        assert_eq!(
            ids(&here),
            ids(&there),
            "the same desktop from any thread: {here:?} vs {there:?}"
        );
    }
}
