fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Dile a tonic-build que use el compilador de Protobuf interno (compilado de fuente)
    std::env::set_var("PROTOC", protobuf_src::protoc());
    
    tonic_build::compile_protos("proto/bittice.proto")?;
    Ok(())
}
