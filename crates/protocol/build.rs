//! Generates the wire (`lan_workspace.v1`) and IPC (`lan_workspace.ipc.v1`)
//! Protobuf modules from `proto/*.proto`.
//!
//! `protox` compiles the descriptors in pure Rust, so no `protoc` binary is
//! required on any build host; `prost-build` then emits the Rust modules from
//! the descriptor set (`compile_fds`). The `.proto` files under `proto/` are
//! copies of the design drafts in `docs/` and are the in-repo source of truth
//! for code generation; keep the two in sync.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/protocol.proto");
    println!("cargo:rerun-if-changed=proto/ipc.proto");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);

    let fds = protox::compile(["proto/protocol.proto", "proto/ipc.proto"], ["proto"])?;

    prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_fds(fds)?;

    Ok(())
}
