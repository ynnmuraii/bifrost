//! Non-blocking process-exit probes for ordered desktop restarts.
use std::io;

/// A missing process has exited; access failures are not proof of exit.
#[cfg(windows)]
pub fn process_has_exited(pid: u32) -> io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: no pointers are supplied, and the returned handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    let mut code = 0;
    // SAFETY: process is an owned live handle and code is valid output storage.
    let success = unsafe { GetExitCodeProcess(process, &mut code) };
    let error = (success == 0).then(io::Error::last_os_error);
    // SAFETY: this handle was returned by OpenProcess and is closed exactly once.
    unsafe { CloseHandle(process) };
    match error {
        Some(error) => Err(error),
        None => Ok(code != STILL_ACTIVE as u32),
    }
}

/// Probe a NUL-terminated pipe name without creating a client connection.
#[cfg(windows)]
pub fn named_pipe_is_released(name: &[u16]) -> bool {
    use windows_sys::Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Pipes::WaitNamedPipeW};
    if name.last() != Some(&0) {
        return false;
    }
    // SAFETY: the slice is terminated, remains alive for the call, and no pointer is retained.
    let available = unsafe { WaitNamedPipeW(name.as_ptr(), 1) };
    available == 0 && io::Error::last_os_error().raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32)
}

#[cfg(unix)]
pub fn process_has_exited(pid: u32) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // An unreaped child has exited and released its listeners, though kill(0) succeeds.
        if stat
            .rsplit_once(')')
            .is_some_and(|(_, tail)| matches!(tail.split_whitespace().next(), Some("Z" | "X")))
        {
            return Ok(true);
        }
    }
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("Invalid process id"))?;
    if pid <= 0 {
        return Err(io::Error::other("Invalid process id"));
    }
    // SAFETY: signal 0 only probes existence; it never delivers a signal.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_process_has_not_exited() {
        assert!(!super::process_has_exited(std::process::id()).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_but_unreaped_child_does_not_block_restart() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !super::process_has_exited(child.id()).unwrap() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the exited child must be recognized before its parent reaps it");
            }
            std::thread::yield_now();
        }
        child.wait().unwrap();
    }
}
