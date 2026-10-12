//! The agent's life is tied to core's by the OS, not only by the agent's own watchdog.
//!
//! `docs/03-architecture.md` S3: «агент замечает EOF core и освобождает» — the agent watches
//! the control channel and releases the user's keys, buttons and capture when core goes away.
//! That is what normally happens (measured on this host: after a force-killed core, no
//! `lws-agent` process is left), but it rests entirely on the agent *deciding* to exit: a core
//! killed with `TerminateProcess` never runs [`crate::agent::Supervisor::stop`], and an agent
//! that missed its exit path — stuck in a blocking backend call, before its watchdog armed, or
//! simply buggy — would keep running with the user's input hooks attached, invisible to the
//! user. Windows can make that impossible: a process in a Job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is killed by the kernel when the job's last handle
//! closes, and core holds that handle. The agent therefore cannot outlive core, whatever the
//! agent's own code does.

/// Keeps the agent in a job that dies with this handle.
///
/// Drop it (core shutting down, or a new agent for the next launch) and the agent in it is
/// killed by the kernel. Attaching can fail — a core started inside another job that does not
/// allow nesting is the common case — and then the agent's own watchdog is the only release
/// path, which is why the failure is a warning and not an error.
#[cfg(windows)]
pub struct KillOnCoreExit(usize);

/// See the Windows type; the guard is a no-op where there are no job objects.
#[cfg(not(windows))]
pub struct KillOnCoreExit;

#[cfg(windows)]
impl KillOnCoreExit {
    /// Puts `child` in a fresh job with kill-on-close.
    pub fn attach(child: &tokio::process::Child) -> std::io::Result<Self> {
        use std::io;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: `CreateJobObjectW` with no name and no attributes returns a new handle or
        // null; nothing else is read or written by this call.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let guard = JobHandle(job);

        // SAFETY: every field is an integer or an IO counters struct that `zeroed` is valid
        // for; only `LimitFlags` is set, which is what the call reads.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `limits` is a live local of the size passed in, and the class matches its
        // type (`JobObjectExtendedLimitInformation` ⇄ `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`).
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let process = child.raw_handle().ok_or_else(|| {
            io::Error::other("the agent process handle is gone before the job is attached")
        })?;
        // SAFETY: both handles are live — the job from above and the child's process handle,
        // taken from a live `Child` — and the call only links them.
        let ok = unsafe { AssignProcessToJobObject(job, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // The guard's handle is moved into `Self`; the local one is forgotten, not closed.
        let raw = guard.into_raw();
        Ok(Self(raw))
    }
}

#[cfg(not(windows))]
impl KillOnCoreExit {
    /// Nothing to do: Unix kills the agent through [`crate::agent::Supervisor::stop`], and a
    /// process there cannot outlive its parent by accident the way a detached Windows child
    /// can (the agent's watchdog still covers a force-killed core).
    pub fn attach(_child: &tokio::process::Child) -> std::io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for KillOnCoreExit {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // SAFETY: the handle came from `CreateJobObjectW` and is closed exactly once; closing
        // the last handle to a `KILL_ON_JOB_CLOSE` job is what kills the agent.
        let _ = unsafe { CloseHandle(self.0 as *mut core::ffi::c_void) };
    }
}

/// A job handle that closes itself if the attach sequence fails halfway.
#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl JobHandle {
    /// Gives up ownership of the handle.
    fn into_raw(self) -> usize {
        let raw = self.0 as usize;
        std::mem::forget(self);
        raw
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` and is closed exactly once.
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
