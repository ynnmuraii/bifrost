//! Core-owned update ordering; HTTP and release verification stay in the updater role.
use super::*;
use protocol::{ipc, wire};
const RATE: Duration = Duration::from_secs(600);
const DEADLINE: Duration = Duration::from_secs(300);
pub(crate) static REEXEC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) static RESTART_TRAY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(crate) static TARGET: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub(crate) fn desktop_args(paths: &config::Paths) -> Vec<std::ffi::OsString> {
    let mut args = Vec::with_capacity(8);
    for (flag, path) in [
        ("--config-dir", Some(paths.config_dir())),
        ("--data-dir", Some(paths.data_dir())),
        ("--state-dir", Some(paths.state_dir())),
        ("--runtime-dir", paths.runtime_dir()),
    ] {
        if let Some(path) = path {
            args.push(flag.into());
            args.push(path.as_os_str().to_owned());
        }
    }
    args
}
fn worker_exit_error(stopping: bool, status: impl std::fmt::Display) -> Option<String> {
    (!stopping).then(|| format!("Updater exited before restart: {status}"))
}
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(windows)]
    {
        bifrost_desktop::process_has_exited(pid).is_ok_and(|exited| !exited)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}
fn live_coordinator(
    coordinator: Option<(u32, Instant)>,
    now: Instant,
    alive: impl FnOnce(u32) -> bool,
) -> Option<u32> {
    coordinator
        .filter(|(_, seen)| now.saturating_duration_since(*seen) < Duration::from_secs(15))
        .map(|(pid, _)| pid)
        .filter(|pid| alive(*pid))
}
fn gui_restart_owner(coordinator: Option<u32>) -> bool {
    let gui = coordinator;
    #[cfg(target_os = "linux")]
    {
        let registered = gui.is_some();
        let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") else {
            return registered;
        };
        let unit = cgroup
            .lines()
            .filter_map(|line| line.splitn(3, ':').nth(2))
            .flat_map(|path| path.split('/'))
            .rfind(|name| name.ends_with(".service") && !name.starts_with("user@"));
        let Some(unit) = unit else {
            return registered;
        };
        let Ok(output) = std::process::Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=MainPID",
                "--value",
                "--",
                unit,
            ])
            .output()
        else {
            return registered;
        };
        if !output.status.success() {
            return registered;
        }
        let Some(pid) = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            return registered;
        };
        // Never make the supervised core's MainPID exit into a detached replacement.
        if pid == std::process::id() {
            return false;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return registered;
        };
        let mut args = cmdline.split(|b| *b == 0).filter(|part| !part.is_empty());
        let Some(exe) = args.next() else {
            return registered;
        };
        let bifrost = std::path::Path::new(std::str::from_utf8(exe).unwrap_or(""))
            .file_name()
            .is_some_and(|name| name == "bifrost");
        registered || (bifrost && args.next().is_none_or(|role| role == b"gui"))
    }
    #[cfg(not(target_os = "linux"))]
    gui.is_some()
}
#[derive(Default)]
pub(super) struct Updates {
    pub peers: BTreeMap<NodeId, (String, bool)>,
    pub allow: bool,
    pub rows: BTreeMap<NodeId, ipc::NodeUpdate>,
    pub notice: String,
    pub coordinator: Option<(u32, Instant)>,
    requested: BTreeMap<NodeId, Instant>,
    sent: BTreeMap<NodeId, Instant>,
    batch: Option<(String, Instant)>,
    worker: Option<(String, Option<NodeId>, std::process::Child, Instant)>,
}
fn terminal(status: &str) -> bool {
    matches!(status, "done" | "refused" | "unreachable")
}
fn peers_complete(rows: &BTreeMap<NodeId, ipc::NodeUpdate>) -> bool {
    rows.values().all(|row| terminal(&row.status))
}
fn reconnect(row: &mut ipc::NodeUpdate, version: &str) {
    if terminal(&row.status) {
        return;
    }
    if row.target_version == version {
        row.status = "done".into();
        row.detail = "Reconnected with target version".into();
    } else if row.status == "restarting" && version.is_empty() {
        row.status = "done".into();
        row.detail = "Verified installation; authenticated reconnect; older release cannot advertise runtime version".into();
    } else if row.status == "restarting" {
        row.status = "refused".into();
        row.detail = format!("Reconnected with unexpected version {version}");
    }
}
fn admission(
    allowed: bool,
    trusted: bool,
    capable: bool,
    busy: bool,
    last: Option<Instant>,
    now: Instant,
    target: &str,
) -> Result<(), &'static str> {
    if !trusted || !capable {
        return Err("Trusted connected RemoteUpdate peer required");
    }
    if !allowed {
        return Err("Remote updates are disabled");
    }
    if busy {
        return Err("Another update is already active");
    }
    if protocol::sanitize_app_version(target) != target || target.is_empty() {
        return Err("Invalid target version");
    }
    if last.is_some_and(|last| now.saturating_duration_since(last) < RATE) {
        return Err("Remote update request rate limited (10 minutes)");
    }
    Ok(())
}
fn headless_command(
    exe: &std::path::Path,
    target: &str,
    paths: &config::Paths,
) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .args(["update", "--to", target])
        .arg("--config-dir")
        .arg(paths.config_dir())
        .arg("--data-dir")
        .arg(paths.data_dir())
        .arg("--state-dir")
        .arg(paths.state_dir());
    if let Some(runtime) = paths.runtime_dir() {
        command.arg("--runtime-dir").arg(runtime);
    }
    command
}

impl Session {
    pub(crate) fn set_update_policy(&mut self, allow: bool) {
        self.updates.allow = allow;
    }
    pub(super) fn stop_update_worker(&mut self) {
        if let Some((_, _, mut child, _)) = self.updates.worker.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    pub(super) async fn start_update_all(&mut self, target: String) -> Result<(), String> {
        if self.updates.batch.is_some() || self.updates.worker.is_some() {
            return Err("An update is already active".into());
        }
        if target.is_empty() || protocol::sanitize_app_version(&target) != target {
            return Err("Invalid target version".into());
        }
        self.updates.rows.clear();
        self.updates.notice = "Updating peers before this node".into();
        let online = self.manager.members().await;
        for record in self.trust.peers.iter().filter(|record| record.is_active()) {
            let peer = record.node_id;
            let (version, capable) = self.updates.peers.get(&peer).cloned().unwrap_or_default();
            let (status, detail) = if !online.contains(&peer) {
                ("unreachable", "Peer is offline")
            } else if !capable {
                ("refused", "Older peer requires a manual update")
            } else if version == target {
                ("done", "Already running target version")
            } else if self
                .updates
                .sent
                .get(&peer)
                .is_some_and(|last| Instant::now().saturating_duration_since(*last) < RATE)
            {
                ("refused", "Remote update request rate limited (10 minutes)")
            } else {
                ("pending", "Waiting for peer")
            };
            self.updates.rows.insert(
                peer,
                ipc::NodeUpdate {
                    node_id: crate::command::node_id_hex(&peer),
                    display_name: record.display_name.clone(),
                    target_version: target.clone(),
                    status: status.into(),
                    detail: detail.into(),
                },
            );
        }
        self.updates.batch = Some((target.clone(), Instant::now()));
        let pending: Vec<_> = self
            .updates
            .rows
            .iter()
            .filter(|(_, row)| row.status == "pending")
            .map(|(peer, _)| *peer)
            .collect();
        for peer in pending {
            self.updates.sent.insert(peer, Instant::now());
            let sent = if let Some(handle) = self.manager.handle(&peer).await {
                handle
                    .send_update_request(wire::UpdateRequest {
                        target_version: target.clone(),
                    })
                    .await
                    .is_ok()
            } else {
                false
            };
            if !sent {
                if let Some(row) = self.updates.rows.get_mut(&peer) {
                    row.status = "unreachable".into();
                    row.detail = "Peer disconnected before request".into();
                }
            }
        }
        Ok(())
    }
    pub(super) async fn remote_update(&mut self, peer: NodeId, request: wire::UpdateRequest) {
        let now = Instant::now();
        let capable = self.layout_connections.contains_key(&peer)
            && self
                .updates
                .peers
                .get(&peer)
                .is_some_and(|(_, capable)| *capable);
        let verdict = admission(
            self.updates.allow,
            self.trust.is_trusted(&peer),
            capable,
            self.updates.worker.is_some() || self.updates.batch.is_some(),
            self.updates.requested.get(&peer).copied(),
            now,
            &request.target_version,
        );
        if let Err(detail) = verdict {
            self.send_update_result(peer, &request.target_version, "refused", detail)
                .await;
            return;
        }
        self.updates.requested.insert(peer, now);
        match self.launch_update(request.target_version.clone(), Some(peer)) {
            Ok(()) => {
                self.updates.notice = self.remote_update_notice(
                    peer,
                    &request.target_version,
                    "Checking official latest release",
                );
                self.send_update_result(
                    peer,
                    &request.target_version,
                    "pending",
                    "Checking official latest release",
                )
                .await;
            }
            Err(detail) => {
                self.send_update_result(peer, &request.target_version, "refused", &detail)
                    .await
            }
        }
    }
    fn remote_update_notice(&self, peer: NodeId, target: &str, detail: &str) -> String {
        let name = self
            .trust
            .get(&peer)
            .map(|record| record.display_name.as_str())
            .unwrap_or("trusted peer");
        format!(
            "remote-update\t{}\t{target}\t{detail}",
            name.replace(['\t', '\r', '\n'], " ")
        )
    }
    async fn send_update_result(&self, peer: NodeId, target: &str, status: &str, detail: &str) {
        if !self.trust.is_trusted(&peer)
            || !self
                .updates
                .peers
                .get(&peer)
                .is_some_and(|(_, capable)| *capable)
        {
            return;
        }
        if let Some(handle) = self.manager.handle(&peer).await {
            let _ = handle
                .send_update_result(wire::UpdateResult {
                    target_version: target.into(),
                    status: status.into(),
                    detail: detail.chars().take(512).collect(),
                })
                .await;
        }
    }
    pub(super) fn update_result(&mut self, peer: NodeId, result: wire::UpdateResult) {
        if !self.trust.is_trusted(&peer) {
            return;
        }
        if let Some(row) = self.updates.rows.get_mut(&peer) {
            if row.target_version != result.target_version || terminal(&row.status) {
                return;
            }
            // A remote assertion cannot prove installation: only a fresh Hello does.
            if matches!(
                result.status.as_str(),
                "pending" | "downloading" | "restarting" | "refused"
            ) {
                row.status = result.status;
                row.detail = result.detail.chars().take(512).collect();
            }
        }
    }
    pub(super) fn update_reconnected(&mut self, peer: NodeId, version: &str) {
        if let Some(row) = self.updates.rows.get_mut(&peer) {
            reconnect(row, version);
        }
    }
    fn launch_update(&mut self, target: String, requester: Option<NodeId>) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let _ = TARGET.set(exe.clone());
        let mut command = headless_command(&exe, &target, &self.paths);
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        self.updates.rows.insert(
            self.node_id,
            ipc::NodeUpdate {
                node_id: crate::command::node_id_hex(&self.node_id),
                display_name: self.display_name.clone(),
                target_version: target.clone(),
                status: "pending".into(),
                detail: "Checking official latest release".into(),
            },
        );
        self.updates.worker = Some((target, requester, child, Instant::now()));
        Ok(())
    }
    pub(super) async fn update_progress(&mut self, progress: ipc::UpdateProgress) -> bool {
        if progress.status == "coordinator" && progress.target_version.is_empty() {
            self.updates.coordinator = progress
                .detail
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid != 0)
                .map(|pid| (pid, Instant::now()));
            return self.updates.coordinator.is_some();
        }
        let Some((target, requester, _, _)) = &self.updates.worker else {
            return false;
        };
        if *target != progress.target_version
            || !matches!(progress.status.as_str(), "downloading" | "refused")
        {
            return false;
        }
        let requester = *requester;
        self.updates.notice = if let Some(peer) = requester {
            self.remote_update_notice(peer, &progress.target_version, &progress.detail)
        } else {
            progress.detail.clone()
        };
        if let Some(peer) = requester {
            self.send_update_result(
                peer,
                &progress.target_version,
                &progress.status,
                &progress.detail,
            )
            .await;
        }
        if let Some(row) = self.updates.rows.get_mut(&self.node_id) {
            row.status = progress.status;
            row.detail = progress.detail;
        }
        true
    }
    pub(super) async fn update_ready(&mut self, target: &str) -> bool {
        let Some((expected, requester, _, _)) = &self.updates.worker else {
            return false;
        };
        if expected != target {
            return false;
        }
        let requester = *requester;
        let coordinator = live_coordinator(self.updates.coordinator, Instant::now(), process_alive);
        let gui_parent = gui_restart_owner(coordinator);
        if !gui_parent {
            let stopped = tokio::task::spawn_blocking(|| {
                bifrost_desktop::stop_tray_and_wait(Duration::from_secs(10))
            })
            .await;
            match stopped {
                Ok(Ok(was_running)) => {
                    RESTART_TRAY.store(was_running, std::sync::atomic::Ordering::Release)
                }
                error => {
                    let detail = format!("Cannot stop the old tray before update: {error:?}");
                    self.updates.notice = detail.clone();
                    if let Some(row) = self.updates.rows.get_mut(&self.node_id) {
                        row.status = "refused".into();
                        row.detail = detail.clone();
                    }
                    if let Some(peer) = requester {
                        self.send_update_result(peer, target, "refused", &detail)
                            .await;
                    }
                    return false;
                }
            }
        }
        // No role may restart while the old agent still owns capture or injected input.
        self.agent.shutdown().await;
        if let Some(peer) = requester {
            self.send_update_result(
                peer,
                target,
                "restarting",
                "Verified replacement installed; capture released",
            )
            .await;
        }
        if let Some(row) = self.updates.rows.get_mut(&self.node_id) {
            row.status = "restarting".into();
            row.detail = "Verified replacement installed".into();
        }
        self.updates.notice = if gui_parent {
            format!("restart-gui:{target}")
        } else if coordinator.is_some() {
            format!("restart-gui-core:{target}")
        } else {
            format!("Restarting into {target}")
        };
        self.publish_gui_state().await;
        // Allow the writer task to flush the final status before listener teardown.
        tokio::time::sleep(Duration::from_millis(100)).await;
        REEXEC.store(!gui_parent, std::sync::atomic::Ordering::Release);
        self.stopping = true;
        true
    }
    pub(super) async fn service_updates(&mut self) {
        if self.stopping {
            return;
        }
        let now = Instant::now();
        if self
            .updates
            .batch
            .as_ref()
            .is_some_and(|(_, started)| now.saturating_duration_since(*started) >= DEADLINE)
        {
            for row in self
                .updates
                .rows
                .values_mut()
                .filter(|row| !terminal(&row.status))
            {
                row.status = "unreachable".into();
                row.detail = "Timed out waiting for target-version reconnect".into();
            }
        }
        if self.updates.batch.is_some() && peers_complete(&self.updates.rows) {
            let (target, _) = self.updates.batch.take().unwrap();
            if target == protocol::app_version() {
                self.updates.notice =
                    "Peer update batch finished; this node is already current".into();
            } else if let Err(error) = self.launch_update(target, None) {
                self.updates.notice = error;
            }
        }
        let failure = self
            .updates
            .worker
            .as_mut()
            .and_then(|(_, _, child, started)| {
                if now.saturating_duration_since(*started) >= DEADLINE {
                    let _ = child.kill();
                    return Some("Local updater timed out".to_owned());
                }
                match child.try_wait() {
                    Ok(Some(status)) => worker_exit_error(false, status),
                    Err(e) => Some(e.to_string()),
                    _ => None,
                }
            });
        if let Some(detail) = failure {
            let detail = self
                .updates
                .rows
                .get(&self.node_id)
                .filter(|row| row.status == "refused")
                .map(|row| row.detail.clone())
                .unwrap_or(detail);
            if let Some((target, requester, mut child, _)) = self.updates.worker.take() {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(peer) = requester {
                    self.send_update_result(peer, &target, "refused", &detail)
                        .await;
                }
            }
            self.updates.notice = detail;
            if let Some(row) = self.updates.rows.get_mut(&self.node_id) {
                row.status = "refused".into();
                row.detail = self.updates.notice.clone();
            }
        }
    }
    pub(super) fn update_line(&self) -> String {
        let rows = self
            .updates
            .rows
            .values()
            .map(|r| {
                format!(
                    "{}:{}:{}:{}",
                    r.node_id,
                    r.target_version,
                    r.status,
                    r.detail.replace(['\r', '\n'], " ")
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "app_version={} allow_remote_update={} notice={} nodes={rows}",
            protocol::app_version(),
            self.updates.allow,
            self.updates.notice.replace(['\r', '\n'], " ")
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_restore_arguments_preserve_all_resolved_paths_and_omit_missing_runtime() {
        let paths = config::Paths::new("config folder", "data folder", "state folder", None);
        assert_eq!(
            desktop_args(&paths),
            [
                "--config-dir",
                "config folder",
                "--data-dir",
                "data folder",
                "--state-dir",
                "state folder"
            ]
            .map(std::ffi::OsString::from)
        );
    }
    #[test]
    fn headless_role_command_preserves_version_and_every_path_without_a_shell() {
        let paths = config::Paths::new(
            "config folder",
            "data folder",
            "state folder",
            Some("run folder".into()),
        );
        let command = headless_command(
            std::path::Path::new("binary folder/bifrost"),
            "0.2.0",
            &paths,
        );
        assert_eq!(command.get_program(), "binary folder/bifrost");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "update",
                "--to",
                "0.2.0",
                "--config-dir",
                "config folder",
                "--data-dir",
                "data folder",
                "--state-dir",
                "state folder",
                "--runtime-dir",
                "run folder",
            ]
        );
    }

    #[test]
    fn acknowledged_worker_exit_never_becomes_a_refusal() {
        assert!(worker_exit_error(true, "exit status: 0").is_none());
        assert!(worker_exit_error(false, "exit status: 0").is_some());
        assert!(worker_exit_error(false, "exit status: 1").is_some());
    }
    #[test]
    fn expired_or_exited_gui_cannot_take_over_core_restart() {
        let now = Instant::now();
        assert_eq!(live_coordinator(Some((42, now)), now, |_| true), Some(42));
        assert_eq!(live_coordinator(Some((42, now)), now, |_| false), None);
        assert_eq!(
            live_coordinator(Some((42, now)), now + Duration::from_secs(15), |_| true),
            None
        );
        assert_eq!(live_coordinator(None, now, |_| true), None);
    }
    #[test]
    fn local_install_waits_for_every_peer_terminal_including_reconnect() {
        let a = NodeId::new([1; 32]);
        let b = NodeId::new([2; 32]);
        let mut rows = BTreeMap::from([
            (
                a,
                ipc::NodeUpdate {
                    target_version: "1.2.3".into(),
                    status: "restarting".into(),
                    ..Default::default()
                },
            ),
            (
                b,
                ipc::NodeUpdate {
                    status: "refused".into(),
                    ..Default::default()
                },
            ),
        ]);
        assert!(!peers_complete(&rows));
        reconnect(rows.get_mut(&a).unwrap(), "1.2.3");
        assert!(peers_complete(&rows));
        assert_eq!(rows[&a].status, "done");
        rows.get_mut(&a).unwrap().status = "downloading".into();
        assert!(!peers_complete(&rows));
        rows.get_mut(&a).unwrap().status = "unreachable".into();
        assert!(peers_complete(&rows));
    }
    #[test]
    fn legacy_reconnect_needs_verified_restart_and_never_claims_runtime_version() {
        let mut row = ipc::NodeUpdate {
            target_version: "1.2.3".into(),
            status: "pending".into(),
            ..Default::default()
        };
        reconnect(&mut row, "");
        assert_eq!(row.status, "pending");
        row.status = "restarting".into();
        reconnect(&mut row, "");
        assert_eq!(row.status, "done");
        assert!(row.detail.contains("cannot advertise runtime version"));
        row.status = "restarting".into();
        reconnect(&mut row, "1.2.2");
        assert_eq!(row.status, "refused");
    }
    #[test]
    fn terminal_outcomes_exclude_restart_claims() {
        for status in ["pending", "downloading", "restarting"] {
            assert!(!terminal(status));
        }
        for status in ["done", "refused", "unreachable"] {
            assert!(terminal(status));
        }
    }
    #[test]
    fn permission_trust_capability_busy_and_monotonic_rate_are_required() {
        let now = Instant::now();
        assert!(admission(true, true, true, false, None, now, "1.2.3").is_ok());
        for (allow, trust, cap, busy) in [
            (false, true, true, false),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, true),
        ] {
            assert!(admission(allow, trust, cap, busy, None, now, "1.2.3").is_err());
        }
        assert!(admission(true, true, true, false, Some(now), now, "1.2.3").is_err());
        assert!(admission(true, true, true, false, Some(now), now + RATE, "1.2.3").is_ok());
        assert!(admission(true, true, true, false, None, now, "https://evil").is_err());
    }
}
