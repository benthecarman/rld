#![allow(clippy::enum_variant_names)]
#![allow(clippy::large_enum_variant)]

use crate::config::Config;
use crate::logger::RldLogger;
use crate::models::MIGRATIONS;
use crate::node::Node;
use crate::proto::lightning_server::LightningServer;
use bip39::Mnemonic;
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::Network;
use clap::Parser;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use diesel_migrations::MigrationHarness;
use lightning::util::logger::Logger;
use lightning::{log_error, log_info};
use macaroon::{ByteString, Format, Macaroon, MacaroonKey, Verifier};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Status};

mod chain;
mod cli;
mod config;
mod events;
mod fees;
mod keys;
mod logger;
mod models;
mod node;
mod onchain;
mod server;

pub mod proto {
    tonic::include_proto!("lnrpc");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let config: Config = Config::parse();

    let logger = Arc::new(RldLogger::default());

    // Create the datadir if it doesn't exist
    let path = PathBuf::from(&config.data_dir);
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

    let macaroon_key_path = path.clone().join("macaroon_key");

    let mac_key = match fs::read(&macaroon_key_path) {
        Ok(bytes) => {
            let arr: [u8; 32] = bytes.as_slice().try_into()?;
            MacaroonKey::from(arr)
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            } else {
                log_info!(logger, "Generating admin macaroon");
                let mac_key = MacaroonKey::generate_random();
                let macaroon = Macaroon::create(Some("rld".into()), &mac_key, "admin".into())?;

                fs::write(&macaroon_key_path, mac_key)?;

                let macaroon_base64 = macaroon.serialize(Format::V1)?;
                // base64 decode to get raw bytes
                let macaroon_bytes = base64::decode_config(macaroon_base64, base64::URL_SAFE)?;
                let admin_macaroon_path = path.join("admin.macaroon");
                fs::write(admin_macaroon_path, macaroon_bytes)?;
                mac_key
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
    let node = Node::new(&config, db_pool, logger, stop.clone()).await?;

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
        }

        let _ = tx.send(());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
