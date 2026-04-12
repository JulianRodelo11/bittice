fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use the embedded Protobuf binary for better speed and compatibility
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    
    tonic_build::compile_protos("proto/bittice.proto")?;
    Ok(())
}
