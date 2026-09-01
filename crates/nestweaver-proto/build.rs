fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Regenerate when the schema changes — without this cargo caches the generated code and
    // silently misses proto edits until a `cargo clean -p nestweaver-proto`.
    println!("cargo:rerun-if-changed=../../proto/nestweaver/daemon/v1/service.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // Ubuntu 22.04 ships protoc 3.12, where proto3 optional fields still
        // require this opt-in. Newer protoc versions continue to accept the
        // flag, so keeping it here makes source and release builds portable
        // across the project's declared Linux baseline.
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(
            &["../../proto/nestweaver/daemon/v1/service.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
