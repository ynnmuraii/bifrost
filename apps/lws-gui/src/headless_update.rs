//! Non-GUI role: uses exactly the desktop updater's fixed release source and verifier.
use crate::updater;
use protocol::ipc::{self as wire, gui_frame::Msg};
use std::process::ExitCode;
fn parse(args: &[String]) -> Result<(String, config::Paths), String> {
    let mut flags = config::CliPaths::default();
    let mut target = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--to" {
            if target.is_some() {
                return Err("Duplicate --to".into());
            }
            target = Some(args.get(i + 1).ok_or("--to needs a version")?.clone());
            i += 2;
        } else {
            match flags.take(args, i) {
                config::Taken::Yes(next) => i = next,
                _ => return Err(format!("Unknown update argument {}", args[i])),
            }
        }
    }
    let target = target.ok_or("update --to VERSION is required")?;
    if target.is_empty() || protocol::sanitize_app_version(&target) != target {
        return Err("Invalid target version".into());
    }
    Ok((target, flags.resolve()?))
}
fn policy(release: &updater::Release, target: &str, current: &str) -> Result<(), String> {
    let version = release.version()?;
    if version.to_string() != target {
        return Err(format!(
            "Requested {target}, but official latest is {version}"
        ));
    }
    if !updater::offer(release, current, None, true)? {
        return Err("Target is not strictly newer than this node".into());
    }
    Ok(())
}
async fn report(paths: &config::Paths, message: Msg) -> Result<(), String> {
    let endpoint = ipc::transport::gui_endpoint(paths).map_err(|e| e.to_string())?;
    let stream = ipc::transport::connect(&endpoint)
        .await
        .map_err(|e| e.to_string())?;
    let mut link = ipc::GuiLink::new(stream);
    link.send(1, message).await.map_err(|e| e.to_string())?;
    loop {
        let frame = link.recv().await.map_err(|e| e.to_string())?;
        if frame.request_id == 1 {
            return match frame.msg {
                Some(Msg::OpResult(result)) if result.ok => Ok(()),
                Some(Msg::OpResult(result)) => Err(result.error_code),
                _ => Err("Unexpected updater acknowledgement".into()),
            };
        }
    }
}
pub fn run(args: Vec<String>) -> ExitCode {
    let result = (|| {
        let (target, paths) = parse(&args)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let notify = |status: &str, detail: &str| {
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    report(
                        &paths,
                        Msg::UpdateProgress(wire::UpdateProgress {
                            target_version: target.clone(),
                            status: status.into(),
                            detail: detail.into(),
                        }),
                    ),
                )
                .await
                .map_err(|_| "Updater IPC timed out".to_owned())?
            })
        };
        let install = (|| {
            let exe = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(|e| e.to_string())?;
            let _lock = updater::install_lock(&exe)?;
            notify("downloading", "Checking official latest release")?;
            let release = updater::check()?;
            policy(&release, &target, protocol::app_version())?;
            // prepare authenticates the Ed25519 manifest and payload hash before replacement.
            let bytes = updater::prepare(&release, &mut |_, _| {})?;
            // The lock remains held until core has acknowledged the restart handoff.
            updater::replace(&exe, &bytes).map_err(|e| e.to_string())?;
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    report(
                        &paths,
                        Msg::UpdateReady(wire::UpdateReady {
                            target_version: target.clone(),
                        }),
                    ),
                )
                .await
                .map_err(|_| "Restart acknowledgement timed out".to_owned())?
            })?;
            Ok::<(), String>(())
        })();
        if let Err(error) = &install {
            let _ = notify("refused", error);
        }
        install
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bifrost update: {error}");
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_requires_canonical_version_and_rejects_urls() {
        for args in [
            vec![],
            vec!["--to", "https://invalid"],
            vec!["--to", "1.2.3", "--url", "x"],
            vec!["--to", "1.2.3", "--to", "1.2.4"],
        ] {
            assert!(parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
    }
    #[test]
    fn only_strictly_newer_exact_latest_is_accepted() {
        let release = updater::Release {
            tag_name: "v1.2.3".into(),
            name: None,
            body: None,
            html_url: String::new(),
            assets: vec![],
            draft: false,
            prerelease: false,
        };
        assert!(policy(&release, "1.2.3", "1.2.2").is_ok());
        assert!(policy(&release, "1.2.2", "1.2.1").is_err());
        assert!(policy(&release, "1.2.3", "1.2.3").is_err());
        assert!(policy(&release, "1.2.3", "2.0.0").is_err());
    }
}
