fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "../../../proto";
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile(
            &[format!("{}/trade_command.proto", proto_root)],
            &[proto_root],
        )?;
    println!("cargo:rerun-if-changed={}/trade_command.proto", proto_root);
    Ok(())
}
