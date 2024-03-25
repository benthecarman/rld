#![allow(clippy::enum_variant_names)]
#![allow(clippy::large_enum_variant)]

use bitcoin::secp256k1::PublicKey;
use lightning::util::ser::Writeable;
use proto::lightning_client::LightningClient;
use std::str::FromStr;

mod proto {
    tonic::include_proto!("lnrpc");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = "http://127.0.0.1:9999";
    let mut client = LightningClient::connect(url).await?;

    let pk =
        PublicKey::from_str("02465ed5be53d04fde66c9418ff14a5f2267723810176c9212b722e542dc1afb1b")
            .unwrap();
    let req = proto::ConnectPeerRequest {
        addr: Some(proto::LightningAddress {
            pubkey: pk.to_string(),
            host: "45.79.52.207:9735".to_string(),
        }),
        perm: false,
        timeout: 10,
    };
    let request = tonic::Request::new(req);
    let response = client.connect_peer(request).await?;
    let response = response.into_inner();
    println!("{response:?}");

    let req = proto::WalletBalanceRequest::default();
    let request = tonic::Request::new(req);
    let response = client.wallet_balance(request).await?;
    let response = response.into_inner();
    println!("{response:?}");

    let req = proto::OpenChannelRequest {
        sat_per_vbyte: 1,
        node_pubkey: pk.encode(),
        local_funding_amount: 26_000,
        ..Default::default()
    };
    let request = tonic::Request::new(req);
    let response = client.open_channel_sync(request).await?;
    let response = response.into_inner();
    println!("{response:?}");

    let req = proto::GetInfoRequest::default();
    let request = tonic::Request::new(req);
    let response = client.get_info(request).await?;
    let response = response.into_inner();
    println!("{response:?}");

    Ok(())
}
