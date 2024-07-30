use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "lightning.proto",
        "invoicesrpc/invoices.proto",
        "lndkrpc/lndkrpc.proto",
        "signrpc/signer.proto",
        "walletrpc/walletkit.proto",
    ];

    let dir = PathBuf::from("lnrpc");

    let proto_paths: Vec<_> = protos.iter().map(|proto| dir.join(proto)).collect();

    tonic_build::configure()
        .build_client(false)
        .build_server(true)
        .compile(&proto_paths, &[dir])?;

    Ok(())
}
