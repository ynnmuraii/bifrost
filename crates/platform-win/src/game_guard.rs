//! Read-only, timer-paced foreground ownership detection; no input hooks or GPU work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub fn fullscreen(window: Rect, monitor: Rect, excluded: bool) -> bool {
    !excluded
        && monitor.right > monitor.left
        && monitor.bottom > monitor.top
        && window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}
pub fn confined(clip: Rect, desktop: Rect) -> bool {
    clip.right > clip.left
        && clip.bottom > clip.top
        && (clip.left > desktop.left
            || clip.top > desktop.top
            || clip.right < desktop.right
            || clip.bottom < desktop.bottom)
}

#[derive(Debug, Default)]
pub struct Observation {
    pub fullscreen: bool,
    pub cursor_confined: bool,
    pub presentation: bool,
    pub app: String,
}

#[cfg(windows)]
pub fn observe() -> windows::core::Result<Observation> {
    use windows::Win32::{
        Foundation::{CloseHandle, RECT},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
        },
        System::Threading::{
            GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::{
            Shell::{
                SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE,
                QUNS_RUNNING_D3D_FULL_SCREEN,
            },
            WindowsAndMessaging::{
                GetClassNameW, GetClipCursor, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
                GetWindowThreadProcessId, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
                SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
            },
        },
    };
    fn rect(r: RECT) -> Rect {
        Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
    // SAFETY: all buffers are stack-owned and correctly sized; handles are used only for
    // read-only metadata. Every acquired process handle is closed before returning.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return Ok(Observation::default());
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut class = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class);
        let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        let excluded = pid == GetCurrentProcessId()
            || matches!(
                class.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            );
        let mut observation = Observation::default();
        if !excluded {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
            let mut image = [0u16; 32768];
            let mut length = image.len() as u32;
            let queried = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(image.as_mut_ptr()),
                &mut length,
            );
            let _ = CloseHandle(process);
            queried?;
            let image = String::from_utf16_lossy(&image[..length as usize]);
            let basename = image.rsplit(['\\', '/']).next().unwrap_or("");
            // GUI/core roles have different PIDs; also recognize a renamed executable.
            static OWN_IMAGE: std::sync::LazyLock<Option<String>> =
                std::sync::LazyLock::new(|| {
                    std::env::current_exe()
                        .ok()
                        .map(|path| path.to_string_lossy().into_owned())
                });
            if basename.eq_ignore_ascii_case("bifrost.exe")
                || OWN_IMAGE
                    .as_ref()
                    .is_some_and(|own| image.eq_ignore_ascii_case(own))
            {
                return Ok(observation);
            }
            observation.app = basename.to_owned();
            let mut bounds = RECT::default();
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut bounds as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
            .is_err()
            {
                GetWindowRect(hwnd, &mut bounds)?;
            }
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
            .as_bool()
            {
                return Err(windows::core::Error::from_thread());
            }
            observation.fullscreen = fullscreen(rect(bounds), rect(monitor.rcMonitor), false);
            let notification = SHQueryUserNotificationState()?;
            observation.presentation = matches!(
                notification,
                QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE | QUNS_BUSY
            );
            let mut clip = RECT::default();
            GetClipCursor(&mut clip)?;
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            observation.cursor_confined = confined(
                rect(clip),
                Rect {
                    left: x,
                    top: y,
                    right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
                },
            );
        }
        Ok(observation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fullscreen_rectangles() {
        for monitor in [
            Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            Rect {
                left: -2560,
                top: -100,
                right: 0,
                bottom: 1340,
            },
        ] {
            assert!(fullscreen(monitor, monitor, false));
            assert!(!fullscreen(monitor, monitor, true));
            assert!(!fullscreen(
                Rect {
                    bottom: monitor.bottom - 40,
                    ..monitor
                },
                monitor,
                false
            ));
            assert!(!fullscreen(
                Rect {
                    right: monitor.right - 1,
                    ..monitor
                },
                monitor,
                false
            ));
            assert!(!confined(monitor, monitor));
            assert!(confined(
                Rect {
                    left: monitor.left + 1,
                    ..monitor
                },
                monitor
            ));
            assert!(!confined(
                Rect {
                    right: monitor.left,
                    ..monitor
                },
                monitor
            ));
        }
    }
}
