//! The tray role must not link the GUI renderer, even though the dispatcher does.
#[test]
fn normal_dependencies_do_not_include_a_gui_renderer() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "-p",
            "bifrost-tray",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--locked",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    let names: Vec<_> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(
        names.contains(&"bifrost-tray") && names.contains(&"ipc"),
        "unexpected dependency tree:\n{tree}"
    );
    let denied = ["eframe", "egui", "glow", "bifrost-gui"];
    let found: Vec<_> = names.iter().filter(|name| denied.contains(name)).collect();
    assert!(
        found.is_empty(),
        "tray must remain renderer-free, found {found:?}"
    );
}
