//! Build script compiling `proto/vortex_ipc.proto` via `prost-build` (`P0-T14`).

#![allow(clippy::print_stdout)]

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../proto");
    let proto_file = proto_dir.join("vortex_ipc.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed={}", proto_dir.display());

    let mut config = prost_build::Config::new();
    config.btree_map(["."]);
    config.compile_protos(&[proto_file], &[proto_dir])?;
    Ok(())
}
