use crate::{menu::Action, State};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use tokio::sync::{mpsc, watch};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{CreateBitmap, DeleteObject},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

const CALLBACK: u32 = WM_APP + 1;
const UPDATE: u32 = WM_APP + 2;
const STOP: u32 = WM_APP + 3;
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
use std::cell::RefCell;
struct Window {
    state: watch::Receiver<State>,
    commands: mpsc::UnboundedSender<Action>,
    icon: RefCell<NOTIFYICONDATAW>,
    taskbar_created: u32,
    ready: lws_desktop::TrayReady,
}

// SAFETY: Windows calls this on the creating thread. GWLP_USERDATA points to the
// boxed Window owned by run(), and stays valid until DestroyWindow returns.
unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Window;
        if !data.is_null() {
            // A shared reference only: the nested loop of `TrackPopupMenu` dispatches more
            // messages into this function while an outer call is still live.
            let window = &*data;
            if message == window.taskbar_created {
                window
                    .ready
                    .set(Shell_NotifyIconW(NIM_ADD, &*window.icon.borrow()) != 0);
                return 0;
            }
            match message {
                CALLBACK => {
                    match lparam as u32 {
                        WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                            let _ = window.commands.send(Action::Open);
                        }
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            let items = window.state.borrow().items();
                            let menu = CreatePopupMenu();
                            if menu.is_null() {
                                return 0;
                            }
                            for (index, item) in items.iter().enumerate() {
                                let flags = MF_STRING
                                    | if item.action.is_some() {
                                        MF_ENABLED
                                    } else {
                                        MF_GRAYED
                                    };
                                AppendMenuW(menu, flags, index + 1, wide(&item.label).as_ptr());
                            }
                            SetMenuDefaultItem(menu, 1, 0);
                            let mut point: POINT = zeroed();
                            GetCursorPos(&mut point);
                            SetForegroundWindow(hwnd);
                            let selected = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                point.x,
                                point.y,
                                0,
                                hwnd,
                                null(),
                            ) as usize;
                            DestroyMenu(menu);
                            PostMessageW(hwnd, WM_NULL, 0, 0);
                            if let Some(action) = selected
                                .checked_sub(1)
                                .and_then(|i| items.get(i))
                                .and_then(|item| item.action.clone())
                            {
                                let _ = window.commands.send(action);
                            }
                        }
                        _ => {}
                    }
                    return 0;
                }
                UPDATE => {
                    let latest = window.state.borrow();
                    let text = latest
                        .items()
                        .get(1)
                        .map(|i| i.label.clone())
                        .unwrap_or_else(|| "multideskflow".into());
                    // The icon is borrowed only for these statements, never across a
                    // nested message loop (`TrackPopupMenu` below re-enters this function).
                    let mut icon = window.icon.borrow_mut();
                    icon.szTip.fill(0);
                    for (slot, ch) in icon.szTip.iter_mut().take(127).zip(text.encode_utf16()) {
                        *slot = ch;
                    }
                    Shell_NotifyIconW(NIM_MODIFY, &*icon);
                    return 0;
                }
                STOP | WM_CLOSE => {
                    DestroyWindow(hwnd);
                    return 0;
                }
                WM_DESTROY => {
                    window.ready.set(false);
                    Shell_NotifyIconW(NIM_DELETE, &*window.icon.borrow());
                    PostQuitMessage(0);
                    return 0;
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) fn run(
    state: watch::Receiver<State>,
    commands: mpsc::UnboundedSender<Action>,
    ready: lws_desktop::TrayReady,
) -> Result<(), String> {
    // SAFETY: all native objects are created and destroyed on this thread. Only
    // PostMessageW crosses threads; it does not dereference the window's user data.
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide("LanWorkspaceTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            UnregisterClassW(class.as_ptr(), instance);
            return Err(std::io::Error::last_os_error().to_string());
        }
        let (width, height, rgba) = lws_desktop::assets::icon_rgba();
        let mut bgra = Vec::with_capacity(rgba.len());
        for pixel in rgba.as_chunks::<4>().0 {
            let premultiply =
                |value: u8| ((u16::from(value) * u16::from(pixel[3]) + 127) / 255) as u8;
            bgra.extend_from_slice(&[
                premultiply(pixel[2]),
                premultiply(pixel[1]),
                premultiply(pixel[0]),
                pixel[3],
            ]);
        }
        let color = CreateBitmap(width as i32, height as i32, 1, 32, bgra.as_ptr().cast());
        let mask_bytes = vec![0u8; (width as usize).div_ceil(16) * 2 * height as usize];
        let mask = CreateBitmap(
            width as i32,
            height as i32,
            1,
            1,
            mask_bytes.as_ptr().cast(),
        );
        let info = ICONINFO {
            fIcon: 1,
            hbmColor: color,
            hbmMask: mask,
            ..zeroed()
        };
        let icon = CreateIconIndirect(&info);
        if !color.is_null() {
            DeleteObject(color);
        }
        if !mask.is_null() {
            DeleteObject(mask);
        }
        if icon.is_null() {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), instance);
            return Err("Не удалось создать значок трея".into());
        }
        let mut notify: NOTIFYICONDATAW = zeroed();
        notify.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        notify.hWnd = hwnd;
        notify.uID = 1;
        notify.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        notify.uCallbackMessage = CALLBACK;
        notify.hIcon = icon;
        for (slot, ch) in notify.szTip.iter_mut().zip("multideskflow".encode_utf16()) {
            *slot = ch;
        }
        let window = Box::new(Window {
            state: state.clone(),
            commands,
            icon: RefCell::new(notify),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            ready,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&*window as *const Window) as isize);
        // Explorer may not have a taskbar yet (early autostart): that is not fatal. The
        // `TaskbarCreated` broadcast re-adds the icon, and only then is the tray "ready".
        if Shell_NotifyIconW(NIM_ADD, &*window.icon.borrow()) != 0 {
            window.ready.set(true);
        }
        // A watch receiver blocks without polling; status broadcasts wake the native loop.
        let address = hwnd as usize;
        let watcher = std::thread::Builder::new()
            .name("lws-tray-notify".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    PostMessageW(address as HWND, STOP, 0, 0);
                    return;
                };
                runtime.block_on(async move {
                    let mut state = state;
                    while state.changed().await.is_ok() {
                        PostMessageW(address as HWND, UPDATE, 0, 0);
                    }
                    PostMessageW(address as HWND, STOP, 0, 0);
                });
            });
        if watcher.is_err() {
            DestroyWindow(hwnd);
            DestroyIcon(icon);
            UnregisterClassW(class.as_ptr(), instance);
            return Err("Не удалось запустить уведомления трея".into());
        }
        let mut message: MSG = zeroed();
        let result = loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break result;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        };
        // Wake the IPC worker if the window itself was closed externally.
        let _ = window.commands.send(Action::Exit);
        let _ = watcher.unwrap().join();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        DestroyIcon(icon);
        UnregisterClassW(class.as_ptr(), instance);
        if result == -1 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        }
    }
}
