//! Enforce the audio child seam independently of platform cfg and feature defaults.

use std::process::Command;

fn dependencies(package: &str) -> Vec<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "--locked",
            "-p",
            package,
            "-e",
            "normal",
            "--all-features",
            "--target",
            "all",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect();
    assert!(
        names.iter().any(|name| name == package),
        "missing dependency root {package}"
    );
    names
}

fn assert_absent(package: &str, denied: &[&str]) {
    let names = dependencies(package);
    let found: Vec<&String> = names
        .iter()
        .filter(|name| denied.contains(&name.as_str()))
        .collect();
    assert!(
        found.is_empty(),
        "{package} violates audio dependency directions: {found:?}"
    );
}

#[test]
fn input_stack_never_depends_on_audio_contracts() {
    for package in [
        "bifrost-agent",
        "focus-router",
        "geometry",
        "edgefx",
        "transport",
    ] {
        assert_absent(package, &["audio-model", "audio-backend"]);
    }
}

#[test]
fn core_does_not_link_native_audio_backend_contracts() {
    assert_absent("bifrost-core", &["audio-backend"]);
}

#[test]
fn backend_remains_independent_of_network_and_async_runtime() {
    assert_absent("audio-backend", &["transport", "identity", "tokio"]);
}
