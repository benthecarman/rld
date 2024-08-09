#![allow(clippy::enum_variant_names)]
#![allow(clippy::large_enum_variant)]

use bip39::Mnemonic;
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::Network;
use clap::Parser;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use diesel_migrations::MigrationHarness;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use macaroon::{ByteString, Format, Macaroon, MacaroonKey, Verifier};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rld::config::Config;
use rld::invoicesrpc::invoices_server::InvoicesServer;
use rld::lndkrpc::offers_server::OffersServer;
use rld::lnrpc::lightning_server::LightningServer;
use rld::logger::RldLogger;
use rld::models::MIGRATIONS;
use rld::node::Node;
use rld::routerrpc::router_server::RouterServer;
use rld::signrpc::signer_server::SignerServer;
use rld::walletrpc::wallet_kit_server::WalletKitServer;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Status};

pub mod lnrpc {
    tonic::include_proto!("lnrpc");
}

pub mod invoicesrpc {
    tonic::include_proto!("invoicesrpc");
}

pub mod lndkrpc {
    tonic::include_proto!("lndkrpc");
}

pub mod routerrpc {
    tonic::include_proto!("routerrpc");
}

pub mod signrpc {
    tonic::include_proto!("signrpc");
}

pub mod walletrpc {
    tonic::include_proto!("walletrpc");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let config: Config = Config::parse();

    let logger = Arc::new(RldLogger::default());

    // Create the datadir if it doesn't exist
    let path = config.data_dir()?;
    fs::create_dir_all(path.clone())?;

    let tls_cert_path = path.clone().join("tls.cert");
    let tls_key_path = path.clone().join("tls.key");

    let cert_bytes = fs::read(&tls_cert_path);
    let key_bytes = fs::read(&tls_key_path);

    let (cert, tls_key) = match (cert_bytes, key_bytes) {
        (Ok(cert), Ok(key)) => (cert, key),
        (Err(e1), Err(e2)) => {
            // make sure file not found error
            if e1.kind() != std::io::ErrorKind::NotFound {
                return Err(e1.into());
            } else if e2.kind() != std::io::ErrorKind::NotFound {
                return Err(e2.into());
            } else {
                log_info!(logger, "Generating self-signed certificate");
                // Generate a self-signed certificate and private key
                let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
                let CertifiedKey { cert, key_pair } =
                    generate_simple_self_signed(subject_alt_names)?;

                let pem = cert.pem();
                let cert_bytes = pem.as_bytes();
                fs::write(&tls_cert_path, cert_bytes)?;

                let key_pem = key_pair.serialize_pem();
                let key_bytes = key_pem.as_bytes();
                fs::write(&tls_key_path, key_bytes)?;

                (cert_bytes.to_vec(), key_bytes.to_vec())
            }
        }
        _ => {
            return Err(anyhow::anyhow!(
                "tls.cert and tls.key must be both present or both absent"
            ));
        }
    };

    let tls_config = ServerTlsConfig::new().identity(Identity::from_pem(&cert, &tls_key));

    let network = config.network();
    let keys = KeysFile::read(&path.join("keys.json"), network, &logger)?;
    let seed = keys.seed.to_seed_normalized("");
    let xpriv = Xpriv::new_master(config.network(), &seed)?;

    let derv_path = match network {
        Network::Bitcoin => DerivationPath::from_str("m/1039h/0h/0h/0/0")?,
        _ => DerivationPath::from_str("m/1039h/1h/0h/0/0")?,
    };
    let mac_root_key = xpriv.derive_priv(&Secp256k1::signing_only(), &derv_path)?;
    let mac_key = MacaroonKey::from(&mac_root_key.private_key.secret_bytes());

    match fs::read(path.join("admin.macaroon")) {
        Ok(_) => {}
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            } else {
                log_info!(logger, "Generating admin macaroon");
                let macaroon = Macaroon::create(Some("rld".into()), &mac_key, "admin".into())?;

                let macaroon_base64 = macaroon.serialize(Format::V2)?;
                // base64 decode to get raw bytes
                let macaroon_bytes = base64::decode_config(macaroon_base64, base64::URL_SAFE)?;
                let admin_macaroon_path = path.join("admin.macaroon");
                fs::write(admin_macaroon_path, macaroon_bytes)?;
            }
        }
    };

    // DB management
    let manager = ConnectionManager::<PgConnection>::new(&config.pg_url);
    let db_pool = Pool::builder()
        .max_size(16)
        .test_on_check_out(true)
        .build(manager)
        .expect("Could not build connection pool");

    // run migrations
    log_info!(logger, "Running database migrations");
    let mut connection = db_pool.get()?;
    connection
        .run_pending_migrations(MIGRATIONS)
        .expect("migrations could not run");
    drop(connection);

    // Set up an oneshot channel to handle shutdown signal
    let (tx, rx) = oneshot::channel();

    let stop = Arc::new(AtomicBool::new(false));
    let node = Node::new(config.clone(), xpriv, db_pool, logger, stop.clone()).await?;

    // Spawn a task to listen for shutdown signals
    let l = node.logger.clone();
    tokio::spawn(async move {
        let mut term_signal = signal(SignalKind::terminate())
            .map_err(|e| log_error!(l, "failed to install TERM signal handler: {e}"))
            .unwrap();
        let mut int_signal = signal(SignalKind::interrupt())
            .map_err(|e| {
                log_error!(l, "failed to install INT signal handler: {e}");
            })
            .unwrap();

        tokio::select! {
            _ = term_signal.recv() => {
                log_info!(l, "Received SIGTERM");
            },
            _ = int_signal.recv() => {
                log_info!(l, "Received SIGINT");
            },
            _ = when_true(stop.clone()) => {
                log_info!(l, "Shutting down due to stop signal");
            }
        }

        let _ = tx.send(());
        stop.store(true, Ordering::Relaxed);
    });

    let socket_addr = SocketAddr::from_str(&format!("{}:{}", config.rpc_bind, config.rpc_port))?;
    let server_node = node.clone();
    Server::builder()
        .accept_http1(true)
        .tls_config(tls_config)?
        .layer(tower_http::cors::CorsLayer::permissive())
        .add_service(tonic_web::enable(LightningServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .add_service(tonic_web::enable(OffersServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .add_service(tonic_web::enable(RouterServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .add_service(tonic_web::enable(InvoicesServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .add_service(tonic_web::enable(WalletKitServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .add_service(tonic_web::enable(SignerServer::with_interceptor(
            server_node.clone(),
            move |req| check_auth(req, &mac_key),
        )))
        .serve_with_shutdown(socket_addr, async move {
            if let Err(e) = rx.await {
                log_error!(server_node.logger, "Error receiving shutdown signal: {e}");
            }
        })
        .await?;

    node.stop().await?;

    log_info!(node.logger, "Shut down complete");
    Ok(())
}

fn when_true(switch: Arc<AtomicBool>) -> impl std::future::Future {
    tokio::task::spawn(async move {
        while !switch.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(100)).await;
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeysFile {
    pub seed: Mnemonic,
    pub network: Network,
}

impl KeysFile {
    pub fn read(path: &Path, network: Network, logger: &RldLogger) -> anyhow::Result<Self> {
        match fs::read_to_string(path) {
            Ok(file) => {
                let keys: KeysFile = serde_json::from_str(&file)?;
                if keys.network != network {
                    anyhow::bail!("Network mismatch");
                }
                Ok(keys)
            }
            Err(e) => {
                log_info!(logger, "Keys file not found, creating new one");
                if e.kind() == std::io::ErrorKind::NotFound {
                    let mut entropy = [0; 32];
                    OsRng.fill_bytes(&mut entropy);
                    let seed = Mnemonic::from_entropy(&entropy)?;
                    let keys = KeysFile { seed, network };
                    fs::write(path, serde_json::to_string(&keys)?)?;
                    Ok(keys)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}

fn time_before_verifier(caveat: &ByteString) -> bool {
    if !caveat.0.starts_with(b"time-before ") {
        return false;
    }
    let str_caveat = match std::str::from_utf8(&caveat.0) {
        Ok(s) => s,
        Err(_) => return false,
    };

    match chrono::DateTime::parse_from_rfc3339(&str_caveat[12..]) {
        Ok(_time) => true, // todo probably need to actually check the time
        Err(_) => false,
    }
}

fn check_auth(req: Request<()>, macaroon_key: &MacaroonKey) -> Result<Request<()>, Status> {
    match req.metadata().get("macaroon") {
        Some(t) => {
            let hex = hex::decode(t).map_err(|e| {
                Status::unauthenticated(format!("Invalid macaroon hex encoding: {e}"))
            })?;
            let mac = Macaroon::deserialize_binary(&hex)
                .map_err(|e| Status::unauthenticated(format!("Invalid macaroon encoding: {e}")))?;
            let mut verifier = Verifier::default();
            verifier.satisfy_general(time_before_verifier); // lncli adds this
            match verifier.verify(&mac, macaroon_key, Default::default()) {
                Ok(_) => Ok(req),
                Err(e) => Err(Status::unauthenticated(format!("Invalid macaroon: {e}"))),
            }
        }
        _ => Err(Status::unauthenticated("No valid auth token")),
    }
}
