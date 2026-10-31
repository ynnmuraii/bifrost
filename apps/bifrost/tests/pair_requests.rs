//! Isolated dry cores: no production roots, OS hooks or synthesized desktop input.
mod common;

use common::{Node, Unpaired};
use std::time::{Duration, Instant};

fn wait_line(node: &Node, command: &str, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let line = node.ctl(command).expect("control request");
        if line.contains(expected) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {expected}, got {line}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(name))
        .expect("response field")
}

fn print_evidence(two: &Unpaired) {
    for (name, node) in [("REQUESTER", &two.b), ("TARGET", &two.a)] {
        let logs = std::fs::read_to_string(&node.log).expect("core log");
        for line in logs.lines().filter(|line| {
            line.contains("pairing request:")
                || line.contains("pairing window open")
                || line.contains("six digits")
                || line.contains("pairing committed")
        }) {
            println!("{name} {line}");
        }
    }
}

#[test]
fn request_accept_requires_both_sas_decisions_before_trust() {
    let _guard = common::serial();
    let two = Unpaired::start("pair-request-accept");
    two.wait_ready();
    two.b
        .ctl(&format!(
            "pair-request 127.0.0.1:{} {}",
            two.a_port, two.a_id
        ))
        .expect("request sent");
    let prompt = wait_line(&two.a, "pair-request-prompt", "requester=");
    assert_eq!(field(&prompt, "requester="), two.b_id);
    two.a
        .ctl(&format!("pair-request-answer {} accept", two.b_id))
        .expect("accept prompt");
    wait_line(&two.b, "pair-request-status", "accepted");
    assert_eq!(two.b.ctl("pair-request-cancel").unwrap(), "OK not waiting");
    assert_eq!(
        two.b.ctl("pair-request-status").unwrap(),
        "OK accepted",
        "a late Cancel must not relabel a ceremony that already entered SAS"
    );
    let a = two.a.ctl("pair-prompt").expect("target SAS");
    let b = two.b.ctl("pair-prompt").expect("requester SAS");
    assert_eq!(field(&a, "sas="), field(&b, "sas="));
    assert_eq!(field(&a, "sas=").len(), 6);
    for node in [&two.a, &two.b] {
        let trust =
            std::fs::read_to_string(node.root.join("config/trust.toml")).unwrap_or_default();
        assert!(!trust.contains("[[peers]]"), "Accept alone must not trust");
    }
    std::thread::scope(|scope| {
        let target = scope.spawn(|| {
            two.a
                .ctl(&format!("pair-answer {} approve", field(&a, "ceremony=")))
        });
        let requester = two
            .b
            .ctl(&format!("pair-answer {} approve", field(&b, "ceremony=")))
            .expect("requester approves SAS");
        assert!(requester.contains("paired"), "{requester}");
        let target = target.join().unwrap().expect("target approves SAS");
        assert!(target.contains("paired"), "{target}");
    });
    for (node, peer) in [(&two.a, &two.b_id), (&two.b, &two.a_id)] {
        assert!(std::fs::read_to_string(node.root.join("config/trust.toml"))
            .unwrap()
            .contains(peer));
    }
    print_evidence(&two);
}

#[test]
fn decline_and_cancel_never_open_a_trust_ceremony() {
    let _guard = common::serial();
    let two = Unpaired::start("pair-request-decline");
    two.wait_ready();
    two.b
        .ctl(&format!(
            "pair-request 127.0.0.1:{} {}",
            two.a_port, two.a_id
        ))
        .expect("request sent");
    wait_line(&two.a, "pair-request-prompt", "requester=");
    two.a
        .ctl(&format!("pair-request-answer {} decline", two.b_id))
        .expect("decline prompt");
    wait_line(&two.b, "pair-request-status", "declined");
    assert_eq!(two.a.ctl("pair-request-prompt").unwrap(), "OK none");
    two.b
        .ctl(&format!(
            "pair-request 127.0.0.1:{} {}",
            two.a_port, two.a_id
        ))
        .expect("repeat request sent");
    wait_line(&two.b, "pair-request-status", "declined");
    for node in [&two.a, &two.b] {
        let trust =
            std::fs::read_to_string(node.root.join("config/trust.toml")).unwrap_or_default();
        assert!(!trust.contains("[[peers]]"));
    }
    print_evidence(&two);
}

#[test]
fn cancelling_the_wait_dismisses_the_remote_prompt() {
    let _guard = common::serial();
    let two = Unpaired::start("pair-request-cancel");
    two.wait_ready();
    two.b
        .ctl(&format!(
            "pair-request 127.0.0.1:{} {}",
            two.a_port, two.a_id
        ))
        .expect("request sent");
    wait_line(&two.a, "pair-request-prompt", "requester=");
    two.b.ctl("pair-request-cancel").expect("cancel");
    wait_line(&two.a, "pair-request-prompt", "none");
    assert_eq!(two.b.ctl("pair-request-status").unwrap(), "OK cancelled");
}

#[test]
fn disabled_request_policy_survives_restart_and_refuses_without_prompt() {
    let _guard = common::serial();
    let mut two = Unpaired::start("pair-request-disabled");
    two.wait_ready();
    two.a.stop();
    let path = two.a.root.join("config/config.toml");
    let mut config = config::Config::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config.pairing.accept_requests = false;
    config.save(&path).unwrap();
    two.a.restart_without_input();
    two.wait_ready();
    two.b
        .ctl(&format!(
            "pair-request 127.0.0.1:{} {}",
            two.a_port, two.a_id
        ))
        .unwrap();
    wait_line(&two.b, "pair-request-status", "disabled");
    assert_eq!(two.a.ctl("pair-request-prompt").unwrap(), "OK none");
}

#[cfg(target_os = "linux")]
mod cli {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    struct PairCli {
        root: PathBuf,
        child: Child,
    }

    impl PairCli {
        fn start(args: &[&str]) -> Self {
            let root = super::common::unique_dir("pair-cli-rejection");
            let output = std::fs::File::create(root.join("cli.log")).unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_bifrost"))
                .arg("--root")
                .arg(&root)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::from(output.try_clone().unwrap()))
                .stderr(Stdio::from(output))
                .spawn()
                .expect("pair CLI starts without a core or agent");
            Self { root, child }
        }

        fn output(&self) -> String {
            std::fs::read_to_string(self.root.join("cli.log")).unwrap()
        }

        fn until(&mut self, expected: &str) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !self.output().contains(expected) {
                assert!(
                    self.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                    "waiting for {expected:?}: {}",
                    self.output()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn answer(&mut self, answer: &[u8]) {
            // Keep stdin open so explicit rejection cannot be confused with EOF.
            self.child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(answer)
                .unwrap();
        }

        fn finish(&mut self) -> ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "pairing must terminate promptly: {}",
                    self.output()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for PairCli {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn rejection_and_eof_exit_nonzero_without_trust_on_either_side() {
        for reject_opener in [false, true] {
            for eof in [false, true] {
                let [port] = super::common::distinct_ports();
                let address = format!("127.0.0.1:{port}");
                let mut opener = PairCli::start(&["pair", "--open", "--listen", &address]);
                opener.until("waiting for a peer to dial");
                let mut dialer = PairCli::start(&["pair", "--dial", &address]);
                opener.until("codes match? [y/N]");
                dialer.until("codes match? [y/N]");
                let (rejecting, approving) = if reject_opener {
                    (&mut opener, &mut dialer)
                } else {
                    (&mut dialer, &mut opener)
                };
                approving.answer(b"y\n");
                if eof {
                    drop(rejecting.child.stdin.take());
                } else {
                    rejecting.answer(b"n\n");
                }
                assert_eq!(rejecting.finish().code(), Some(1), "{}", rejecting.output());
                assert!(
                    rejecting
                        .output()
                        .contains("pairing rejected: UserRejected"),
                    "{}",
                    rejecting.output()
                );
                assert_eq!(approving.finish().code(), Some(1), "{}", approving.output());
                assert!(
                    approving
                        .output()
                        .contains("pairing rejected: PeerRejected"),
                    "{}",
                    approving.output()
                );
                for cli in [&opener, &dialer] {
                    assert!(
                        !cli.root.join("config/trust.toml").exists(),
                        "rejection must not write trust: {}",
                        cli.output()
                    );
                    assert!(!cli.output().contains("paired with"), "{}", cli.output());
                }
            }
        }
    }
}
