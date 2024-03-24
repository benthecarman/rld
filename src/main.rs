use crate::config::Config;
use crate::logger::RldLogger;
use crate::models::MIGRATIONS;
use crate::node::Node;
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
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

mod chain;
mod config;
mod events;
mod fees;
mod keys;
mod logger;
mod models;
mod node;
mod onchain;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let config: Config = Config::parse();

    // DB management
    let manager = ConnectionManager::<PgConnection>::new(&config.pg_url);
    let db_pool = Pool::builder()
        .max_size(16)
        .test_on_check_out(true)
        .build(manager)
        .expect("Could not build connection pool");

    // run migrations
    let mut connection = db_pool.get()?;
    connection
        .run_pending_migrations(MIGRATIONS)
        .expect("migrations could not run");
    drop(connection);

    // Set up an oneshot channel to handle shutdown signal
    let (tx, rx) = oneshot::channel();

    let node = Node::new(&config, db_pool.clone()).await?;

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
    });

    // wait for shutdown signal
    rx.await?;

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
        match std::fs::read_to_string(path) {
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
                    std::fs::write(path, serde_json::to_string(&keys)?)?;
                    Ok(keys)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}
