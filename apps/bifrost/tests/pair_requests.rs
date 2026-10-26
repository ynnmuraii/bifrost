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
    let dir = two.dir.clone();
    drop(two);
    std::fs::remove_dir_all(dir).expect("clean isolated roots");
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
    let dir = two.dir.clone();
    drop(two);
    std::fs::remove_dir_all(dir).expect("clean isolated roots");
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
    let dir = two.dir.clone();
    drop(two);
    std::fs::remove_dir_all(dir).expect("clean isolated roots");
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
    let dir = two.dir.clone();
    drop(two);
    std::fs::remove_dir_all(dir).expect("clean isolated roots");
}
