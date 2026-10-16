fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winres::WindowsResource::new()
            .set_icon("assets/icon.ico")
            // One binary serves every role (gui/run/agent/tray), so the description is the
            // bare product name: Task Manager then shows Bifrost for all of them.
            .set("ProductName", "Bifrost")
            .set("FileDescription", "Bifrost")
            .set("InternalName", "Bifrost")
            .set("OriginalFilename", "bifrost.exe")
            .set("FileVersion", env!("CARGO_PKG_VERSION"))
            .set("ProductVersion", env!("CARGO_PKG_VERSION"))
            .compile()
            .expect("failed to embed the Bifrost Windows icon");
    }
}
