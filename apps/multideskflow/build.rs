fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winres::WindowsResource::new()
            .set_icon("assets/icon.ico")
            // One binary serves every role (gui/run/agent/tray), so the description is the
            // bare product name: Task Manager then shows `multideskflow` for all of them.
            .set("ProductName", "multideskflow")
            .set("FileDescription", "multideskflow")
            .set("InternalName", "multideskflow")
            .set("OriginalFilename", "multideskflow.exe")
            .compile()
            .expect("failed to embed the multideskflow Windows icon");
    }
}
