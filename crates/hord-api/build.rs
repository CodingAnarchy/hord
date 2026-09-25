//! Generate the prost types, tonic client and server stubs, and pbjson
//! serde impls from `proto/hord.proto` (ADR 0024). `protoc` is vendored.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptors = out.join("hord_descriptor.bin");
    println!("cargo:rerun-if-changed=proto/hord.proto");
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .file_descriptor_set_path(&descriptors)
        .compile_with_config(config, &["proto/hord.proto"], &["proto"])?;
    let set = std::fs::read(&descriptors)?;
    // Default values are printed too (an empty list is `[]`, not absent),
    // so every message has one stable JSON shape for agents.
    pbjson_build::Builder::new()
        .register_descriptors(&set)?
        .emit_fields()
        .build(&[".hord.v1"])?;
    Ok(())
}
