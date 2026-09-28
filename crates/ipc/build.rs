//! Compiles the daemon contract into Rust types.
//!
//! The generated code is never hand-edited: `proto/midiharbor/v1/harbor.proto` is the contract,
//! and this build step is the only thing that translates it.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../proto/midiharbor/v1/harbor.proto";
    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed=../../proto");

    // Supply a compiler unless one was chosen explicitly. The protobuf toolchain is not vendored
    // by the codegen crates, so without this a machine that happens to have `protoc` installed
    // builds and a clean one does not — which is exactly the difference between a developer's
    // laptop and a fresh checkout.
    if std::env::var_os("PROTOC").is_none()
        && let Ok(path) = protoc_bin_vendored::protoc_bin_path()
    {
        // SAFETY: a build script is single-threaded at this point, so there is no other thread
        // that could observe the environment changing.
        unsafe {
            std::env::set_var("PROTOC", path);
        }
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
