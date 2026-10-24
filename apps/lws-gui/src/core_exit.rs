//! A disconnected GUI is not proof that core has released its listeners.
use ipc::transport;
#[cfg(unix)]
use ipc::transport::Endpoint;
use std::time::{Duration, Instant};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Waiting,
    Exited,
    TimedOut,
}

pub(crate) struct Wait {
    since: Instant,
    pid: Option<u32>,
    #[cfg(unix)]
    endpoints: [Endpoint; 2],
    #[cfg(windows)]
    endpoints: [Vec<u16>; 2],
    #[cfg(unix)]
    runtime: tokio::runtime::Runtime,
}

impl Wait {
    pub(crate) fn new(
        paths: &config::Paths,
        pid: Option<u32>,
        now: Instant,
    ) -> Result<Self, String> {
        let endpoints = [
            transport::ctl_endpoint(paths).map_err(|e| e.to_string())?,
            transport::gui_endpoint(paths).map_err(|e| e.to_string())?,
        ];
        Ok(Self {
            since: now,
            pid,
            #[cfg(unix)]
            endpoints,
            #[cfg(windows)]
            endpoints: endpoints
                .map(|endpoint| endpoint.name().encode_utf16().chain([0]).collect()),
            #[cfg(unix)]
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?,
        })
    }

    pub(crate) fn timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) >= TIMEOUT
    }

    pub(crate) fn poll(&self, now: Instant) -> Step {
        poll(self.since, now, || {
            if self
                .pid
                .is_some_and(|pid| !lws_desktop::process_has_exited(pid).unwrap_or(false))
            {
                return false;
            }
            self.endpoints
                .iter()
                .all(|endpoint| self.endpoint_is_free(endpoint))
        })
    }

    #[cfg(windows)]
    fn endpoint_is_free(&self, endpoint: &[u16]) -> bool {
        lws_desktop::named_pipe_is_released(endpoint)
    }

    #[cfg(unix)]
    fn endpoint_is_free(&self, endpoint: &Endpoint) -> bool {
        self.runtime.block_on(async {
            matches!(
                tokio::time::timeout(Duration::from_millis(1), tokio::net::UnixStream::connect(endpoint.path())).await,
                Ok(Err(error)) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
            )
        })
    }
}

fn poll(since: Instant, now: Instant, probe: impl FnOnce() -> bool) -> Step {
    if probe() {
        Step::Exited
    } else if now.saturating_duration_since(since) >= TIMEOUT {
        Step::TimedOut
    } else {
        Step::Waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_and_half_second_do_not_prove_exit() {
        let start = Instant::now();
        assert_eq!(
            poll(start, start + Duration::from_millis(501), || false),
            Step::Waiting
        );
        assert_eq!(
            poll(start, start + Duration::from_secs(3), || false),
            Step::Waiting
        );
        assert_eq!(
            poll(start, start + Duration::from_secs(4), || true),
            Step::Exited
        );
    }

    #[test]
    fn wait_is_bounded_and_never_assumes_exit_on_timeout() {
        let start = Instant::now();
        assert_eq!(poll(start, start + TIMEOUT, || false), Step::TimedOut);
        assert_eq!(poll(start, start, || true), Step::Exited);
    }
}

/// Preserve supervision only when core is the service's MainPID, not merely a GUI child.
#[cfg(target_os = "linux")]
pub(crate) fn systemd_unit(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let unit = service_from_cgroup(&cgroup)?;
    let output = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=MainPID",
            "--value",
            "--",
            &unit,
        ])
        .output()
        .ok()?;
    (output.status.success()
        && std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?
            == pid)
        .then_some(unit)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn systemd_unit(_pid: u32) -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn service_from_cgroup(cgroup: &str) -> Option<String> {
    cgroup
        .lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .flat_map(|path| path.split('/'))
        .rfind(|name| name.ends_with(".service") && !name.starts_with("user@"))
        .map(str::to_owned)
}

#[cfg(test)]
mod systemd_tests {
    #[test]
    fn only_dedicated_service_names_are_candidates() {
        assert_eq!(super::service_from_cgroup("0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-bifrost@lws1.service"),
            Some("app-bifrost@lws1.service".into()));
        assert_eq!(
            super::service_from_cgroup(
                "0::/user.slice/user@1000.service/app.slice/app-bifrost.scope"
            ),
            None
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[tokio::test]
    async fn pipe_probe_does_not_connect_and_detects_listener_release() {
        let endpoint = transport::Endpoint::pipe(format!(
            r"\\.\pipe\bifrost-exit-probe-test-{}",
            std::process::id()
        ));
        let mut listener = transport::listen(&endpoint).unwrap();
        let name: Vec<u16> = endpoint.name().encode_utf16().chain([0]).collect();
        for _ in 0..3 {
            assert!(!lws_desktop::named_pipe_is_released(&name));
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                listener.accept(&ipc::SameUser::current()),
            )
            .await
            .is_err(),
            "probing must not create a connection for core to accept"
        );
        drop(listener);
        // Windows completes cancellation of the pending accept on a later runtime turn.
        tokio::time::timeout(Duration::from_secs(1), async {
            while !lws_desktop::named_pipe_is_released(&name) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the name is released after the pending accept is cancelled");
    }
}
