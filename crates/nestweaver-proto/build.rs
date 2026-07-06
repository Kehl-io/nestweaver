fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Regenerate when the schema changes — without this cargo caches the generated code and
    // silently misses proto edits until a `cargo clean -p nestweaver-proto`.
    println!("cargo:rerun-if-changed=../../proto/nestweaver/daemon/v1/service.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["../../proto/nestweaver/daemon/v1/service.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
