fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile the Android-emulator gRPC control proto (shipped with the Vega SDK,
    // vendored under proto/). We only need the client side.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/emulator_controller.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/emulator_controller.proto");
    Ok(())
}
