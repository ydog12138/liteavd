fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let mut protos: Vec<_> = std::fs::read_dir("proto")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "proto"))
        .collect();
    protos.sort();
    let mut prost = prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_build::configure()
        .build_server(false)
        .compile_protos_with_config(prost, &protos, &["proto"])?;
    Ok(())
}
