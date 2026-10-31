//! Binding the parent terminal back to stdin/stdout/stderr.
//!
//! On Windows the executable is a *Windows-subsystem* program (`windows_subsystem =
//! "windows"` in `main.rs`), so that opening the window — double-click, `Start-Process`, the
//! Start menu — never shows a console. The price is that such a process starts with no
//! console and no standard handles, even when a terminal launched it. The command-line and
//! agent roles therefore call [`attach_parent`] first: it attaches to the parent's console
//! and gives every standard handle that is *not* already valid (a pipe or file the caller
//! redirected to is valid, and is left alone) the console's own input/output.
//!
//! Known limit of a Windows-subsystem executable: `cmd.exe` and an interactive PowerShell do
//! not wait for it, so the prompt may come back before the output and `%ERRORLEVEL%` /
//! `$LASTEXITCODE` are not set from it. The exit code is right whenever the caller waits
//! (`start /wait`, `| Out-Host`, `Start-Process -Wait`, a script or a program that spawns
//! it, redirected output). `packaging/README_RU.txt` says so.
//!
//! A GUI host that launches this executable directly may supply neither a console-owning
//! parent nor redirected output. CLI commands then log a launch hint and exit with code 69;
//! they must not allocate a new console window or start an invisible pairing ceremony.
//!
//! Processes this program starts for itself (the core by the window, the agent by the core)
//! are created with `CREATE_NO_WINDOW`, so no console appears for them either.

/// Attaches to the parent's console, if there is one. Does nothing on other platforms.
pub fn attach_parent() {
    #[cfg(windows)]
    imp::attach_parent();
}

/// Attaches CLI output or records why a console-less host cannot display the command.
/// Internal child roles keep using `attach_parent` without changing their launch contract.
pub fn attach_cli(args: &[String]) -> Result<(), std::process::ExitCode> {
    attach_parent();
    #[cfg(windows)]
    if !imp::output_available() {
        use std::io::Write;

        if let Ok(paths) = config::CliPaths::parse(args).resolve() {
            let directory = paths.logs_dir();
            if std::fs::create_dir_all(&directory).is_ok() {
                if let Ok(mut log) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(directory.join("bifrost-core.log"))
                {
                    let _ = writeln!(
                        log,
                        "CLI command needs a console: run it from cmd/PowerShell or via cmd /c."
                    );
                }
            }
        }
        return Err(std::process::ExitCode::from(69));
    }
    #[cfg(not(windows))]
    let _ = args;
    Ok(())
}

#[cfg(windows)]
mod imp {
    use std::fs::OpenOptions;
    use std::os::windows::io::IntoRawHandle;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    pub fn output_available() -> bool {
        use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_UNKNOWN};

        [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .any(|slot| {
                // SAFETY: inspects this process's standard output handles without taking ownership.
                let handle = unsafe { GetStdHandle(slot) };
                !handle.is_null()
                    && handle != INVALID_HANDLE_VALUE
                    && unsafe { GetFileType(handle) } != FILE_TYPE_UNKNOWN
            })
    }

    pub fn attach_parent() {
        // SAFETY: plain Win32 calls with constant arguments. Failure means "no parent
        // console" (launched from Explorer, a service, a pipe-only parent) or "already has
        // one" (started with CREATE_NO_WINDOW); both leave the handles below as they are.
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        for (slot, device, writable) in [
            (STD_INPUT_HANDLE, "CONIN$", false),
            (STD_OUTPUT_HANDLE, "CONOUT$", true),
            (STD_ERROR_HANDLE, "CONOUT$", true),
        ] {
            // SAFETY: reads this process's own standard-handle slot.
            let current = unsafe { GetStdHandle(slot) };
            if !current.is_null() && current != INVALID_HANDLE_VALUE {
                // Redirected by the caller (or inherited from a parent that has them).
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(!writable)
                .write(writable)
                .open(device)
            else {
                continue;
            };
            // SAFETY: the handle is valid and now owned by the standard-handle slot, for the
            // rest of the process's life (it is intentionally never closed).
            unsafe {
                SetStdHandle(slot, file.into_raw_handle());
            }
        }
    }
}
