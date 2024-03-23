use crate::chain::TxBroadcaster;
use crate::config::Config;
use crate::events::EventHandler;
use crate::fees::RldFeeEstimator;
use crate::logger::RldLogger;
use crate::models::MIGRATIONS;
use anyhow::anyhow;
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::{BlockHash, Network};
use bitcoincore_rpc::{Auth, RpcApi};
use clap::Parser;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use diesel_migrations::MigrationHarness;
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus, Filter, Watch};
use lightning::events::Event;
use lightning::ln::channelmanager::{
    ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::onion_message::messenger::{DefaultMessageRouter, SimpleArcOnionMessenger};
use lightning::routing::gossip;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScoringFeeParameters;
use lightning::sign::{EntropySource, InMemorySigner, KeysManager};
use lightning::util::config::UserConfig;
use lightning::util::logger::Logger;
use lightning::util::persist;
use lightning::util::persist::{KVStore, MonitorUpdatingPersister};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::{log_error, log_info};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::{init, poll, SpvClient, UnboundedCache};
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

mod chain;
mod config;
mod events;
mod fees;
mod logger;
mod models;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<RldLogger>>;

pub(crate) type GossipVerifier = lightning_block_sync::gossip::GossipVerifier<
    lightning_block_sync::gossip::TokioSpawner,
    Arc<lightning_block_sync::rpc::RpcClient>,
    Arc<RldLogger>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
    SocketDescriptor,
    ChainMonitor,
    TxBroadcaster,
    RldFeeEstimator,
    GossipVerifier,
    RldLogger,
>;

type OnionMessenger =
    SimpleArcOnionMessenger<ChainMonitor, TxBroadcaster, RldFeeEstimator, RldLogger>;

pub(crate) type ChannelManager =
    SimpleArcChannelManager<ChainMonitor, TxBroadcaster, RldFeeEstimator, RldLogger>;

type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<dyn Filter + Send + Sync>,
    Arc<TxBroadcaster>,
    Arc<RldFeeEstimator>,
    Arc<RldLogger>,
    Arc<
        MonitorUpdatingPersister<
            Arc<FilesystemStore>,
            Arc<RldLogger>,
            Arc<KeysManager>,
            Arc<KeysManager>,
        >,
    >,
>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::try_init()?;
    let config: Config = Config::parse();

    // Create the datadir if it doesn't exist
    let path = PathBuf::from(&config.data_dir);
    fs::create_dir_all(path.clone())?;

    let logger = Arc::new(RldLogger::default());

    let keys = KeysFile::read(&path.join("keys.json"), config.network(), &logger)?;

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

    let bitcoind_auth = Auth::UserPass(
        config.bitcoind_rpc_user.clone(),
        config.bitcoind_rpc_password.clone(),
    );
    let bitcoind_endpoint = format!("http://{}:{}", config.bitcoind_host, config.bitcoind_port);
    let bitcoind = Arc::new(bitcoincore_rpc::Client::new(
        &bitcoind_endpoint,
        bitcoind_auth.clone(),
    )?);

    let network = config.network();
    let blockchain_info = bitcoind.get_blockchain_info()?;
    if blockchain_info.chain != network.to_core_arg() {
        anyhow::bail!("Network mismatch");
    }

    let fee_estimator = Arc::new(RldFeeEstimator::new(bitcoind.clone())?);
    let broadcaster = Arc::new(TxBroadcaster::new(bitcoind.clone(), logger.clone()));

    let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let keys_manager = Arc::new(KeysManager::new(
        &keys.seed,
        cur.as_secs(),
        cur.subsec_nanos(),
    ));

    let ldk_data_dir = path.join("ldk");
    let fs_store = Arc::new(FilesystemStore::new(ldk_data_dir.clone()));

    let persister = Arc::new(MonitorUpdatingPersister::new(
        Arc::clone(&fs_store),
        Arc::clone(&logger),
        1_000,
        Arc::clone(&keys_manager),
        Arc::clone(&keys_manager),
    ));

    let chain_monitor: Arc<ChainMonitor> = Arc::new(ChainMonitor::new(
        None,
        Arc::clone(&broadcaster),
        Arc::clone(&logger),
        Arc::clone(&fee_estimator),
        Arc::clone(&persister),
    ));

    let mut channelmonitors =
        persister.read_all_channel_monitors_with_updates(&broadcaster, &fee_estimator)?;

    let auth = base64::encode(format!(
        "{}:{}",
        config.bitcoind_rpc_user, config.bitcoind_rpc_password
    ));
    let endpoint =
        HttpEndpoint::for_host(config.bitcoind_host.clone()).with_port(config.bitcoind_port);
    let block_source = Arc::new(lightning_block_sync::rpc::RpcClient::new(&auth, endpoint)?);

    // Step 8: Poll for the best chain tip, which may be used by the channel manager & spv client
    let polled_chain_tip = init::validate_best_block_header(block_source.as_ref())
        .await
        .expect("Failed to fetch best block header and best block");

    // Step 9: Initialize routing ProbabilisticScorer
    let network_graph_path = ldk_data_dir.clone().join("network_graph");
    let network_graph = Arc::new(models::read_network_graph(
        Path::new(&network_graph_path),
        network,
        logger.clone(),
    ));

    let scorer_path = ldk_data_dir.clone().join("scorer");
    let scorer = Arc::new(RwLock::new(models::read_scorer(
        Path::new(&scorer_path),
        Arc::clone(&network_graph),
        Arc::clone(&logger),
    )));

    // Step 10: Create Router
    // copied Mutiny scoring fee parameters
    let scoring_fee_params = ProbabilisticScoringFeeParameters {
        base_penalty_amount_multiplier_msat: 8192 * 100,
        base_penalty_msat: 100_000,
        liquidity_penalty_multiplier_msat: 30_000 * 15,
        liquidity_penalty_amount_multiplier_msat: 192 * 15,
        historical_liquidity_penalty_multiplier_msat: 10_000 * 15,
        historical_liquidity_penalty_amount_multiplier_msat: 64 * 15,
        ..Default::default()
    };
    let router = Arc::new(DefaultRouter::new(
        network_graph.clone(),
        logger.clone(),
        keys_manager.get_secure_random_bytes(),
        scorer.clone(),
        scoring_fee_params,
    ));

    // Step 11: Initialize the ChannelManager
    let mut user_config = UserConfig::default();
    user_config
        .channel_handshake_limits
        .force_announced_channel_preference = false;
    user_config
        .channel_handshake_config
        .negotiate_anchors_zero_fee_htlc_tx = true;
    user_config.manually_accept_inbound_channels = true;
    let mut restarting_node = true;
    let (channel_manager_blockhash, channel_manager) = {
        if let Ok(mut f) = fs::File::open(ldk_data_dir.join("manager")) {
            let mut channel_monitor_mut_references = Vec::new();
            for (_, channel_monitor) in channelmonitors.iter_mut() {
                channel_monitor_mut_references.push(channel_monitor);
            }
            let read_args = ChannelManagerReadArgs::new(
                keys_manager.clone(),
                keys_manager.clone(),
                keys_manager.clone(),
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                router,
                logger.clone(),
                user_config,
                channel_monitor_mut_references,
            );
            <(BlockHash, ChannelManager)>::read(&mut f, read_args).map_err(|e| {
                log_error!(logger, "Failed to read ChannelManager from disk: {}", e);
                anyhow!(e)
            })?
        } else {
            // We're starting a fresh node.
            restarting_node = false;

            let polled_best_block = polled_chain_tip.to_best_block();
            let polled_best_block_hash = polled_best_block.block_hash();
            let chain_params = ChainParameters {
                network,
                best_block: polled_best_block,
            };
            let fresh_channel_manager = ChannelManager::new(
                fee_estimator.clone(),
                chain_monitor.clone(),
                broadcaster.clone(),
                router,
                logger.clone(),
                keys_manager.clone(),
                keys_manager.clone(),
                keys_manager.clone(),
                user_config,
                chain_params,
                cur.as_secs() as u32,
            );
            (polled_best_block_hash, fresh_channel_manager)
        }
    };

    // Step 12: Sync ChannelMonitors and ChannelManager to chain tip
    let mut chain_listener_channel_monitors = Vec::new();
    let mut cache = UnboundedCache::new();
    let chain_tip = if restarting_node {
        let mut chain_listeners = vec![(
            channel_manager_blockhash,
            &channel_manager as &(dyn lightning::chain::Listen + Send + Sync),
        )];

        for (blockhash, channel_monitor) in channelmonitors.drain(..) {
            let outpoint = channel_monitor.get_funding_txo().0;
            chain_listener_channel_monitors.push((
                blockhash,
                (
                    channel_monitor,
                    broadcaster.clone(),
                    fee_estimator.clone(),
                    logger.clone(),
                ),
                outpoint,
            ));
        }

        for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
            chain_listeners.push((
                monitor_listener_info.0,
                &monitor_listener_info.1 as &(dyn lightning::chain::Listen + Send + Sync),
            ));
        }

        init::synchronize_listeners(block_source.as_ref(), network, &mut cache, chain_listeners)
            .await
            .expect("Failed to synchronize listeners")
    } else {
        polled_chain_tip
    };

    // Step 13: Give ChannelMonitors to ChainMonitor
    for item in chain_listener_channel_monitors.drain(..) {
        let channel_monitor = item.1 .0;
        let funding_outpoint = item.2;
        assert_eq!(
            chain_monitor.watch_channel(funding_outpoint, channel_monitor),
            Ok(ChannelMonitorUpdateStatus::Completed)
        );
    }

    // Step 14: Optional: Initialize the P2PGossipSync
    let gossip_sync = Arc::new(P2PGossipSync::new(
        Arc::clone(&network_graph),
        None,
        Arc::clone(&logger),
    ));

    // Step 15: Initialize the PeerManager
    let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
    let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
        Arc::clone(&keys_manager),
        Arc::clone(&keys_manager),
        Arc::clone(&logger),
        Arc::new(DefaultMessageRouter::new(Arc::clone(&network_graph))),
        Arc::clone(&channel_manager),
        IgnoringMessageHandler {},
    ));
    let mut ephemeral_bytes = [0; 32];
    let current_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    OsRng.fill_bytes(&mut ephemeral_bytes);
    let lightning_msg_handler = MessageHandler {
        chan_handler: channel_manager.clone(),
        route_handler: gossip_sync.clone(),
        onion_message_handler: onion_messenger.clone(),
        custom_message_handler: IgnoringMessageHandler {},
    };
    let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
        lightning_msg_handler,
        current_time.try_into()?,
        &ephemeral_bytes,
        logger.clone(),
        Arc::clone(&keys_manager),
    ));

    // Install a GossipVerifier in in the P2PGossipSync
    let utxo_lookup = GossipVerifier::new(
        Arc::clone(&block_source),
        lightning_block_sync::gossip::TokioSpawner,
        Arc::clone(&gossip_sync),
        Arc::clone(&peer_manager),
    );
    gossip_sync.add_utxo_lookup(Some(utxo_lookup));

    // ## Running LDK
    // Step 16: Initialize networking

    let peer_manager_connection_handler = peer_manager.clone();
    let listening_port = config.port;
    let stop_listen_connect = Arc::new(AtomicBool::new(false));
    let stop_listen = Arc::clone(&stop_listen_connect);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
            .await
            .expect("Failed to bind to listen port - is something else already listening on it?");
        loop {
            let peer_mgr = peer_manager_connection_handler.clone();
            let tcp_stream = listener.accept().await.unwrap().0;
            if stop_listen.load(Ordering::Acquire) {
                return;
            }
            tokio::spawn(async move {
                lightning_net_tokio::setup_inbound(
                    peer_mgr.clone(),
                    tcp_stream.into_std().unwrap(),
                )
                .await;
            });
        }
    });

    // Step 17: Connect and Disconnect Blocks
    let channel_manager_listener = channel_manager.clone();
    let chain_monitor_listener = chain_monitor.clone();
    let bitcoind_block_source = block_source.clone();
    tokio::spawn(async move {
        let chain_poller = poll::ChainPoller::new(bitcoind_block_source.as_ref(), network);
        let chain_listener = (chain_monitor_listener, channel_manager_listener);
        let mut spv_client = SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
        loop {
            spv_client.poll_best_tip().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    // Step 18: Handle LDK Events
    let event_handler = EventHandler {
        channel_manager: channel_manager.clone(),
        fee_estimator: fee_estimator.clone(),
        keys_manager: keys_manager.clone(),
        db_pool: db_pool.clone(),
        logger: logger.clone(),
    };
    let event_handler_func = move |event: Event| {
        let ev = event_handler.clone();
        async move {
            ev.handle_event(event).await;
        }
    };

    // Step 19: Persist ChannelManager and NetworkGraph
    let persister = Arc::new(FilesystemStore::new(ldk_data_dir.clone()));

    // Step 20: Background Processing
    let (bp_exit, bp_exit_check) = tokio::sync::watch::channel(());
    let mut background_processor = tokio::spawn(process_events_async(
        Arc::clone(&persister),
        event_handler_func,
        chain_monitor.clone(),
        channel_manager.clone(),
        GossipSync::p2p(gossip_sync.clone()),
        peer_manager.clone(),
        logger.clone(),
        Some(scorer.clone()),
        move |t| {
            let mut bp_exit_fut_check = bp_exit_check.clone();
            Box::pin(async move {
                tokio::select! {
                    _ = tokio::time::sleep(t) => false,
                    _ = bp_exit_fut_check.changed() => true,
                }
            })
        },
        false,
        || {
            Some(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap(),
            )
        },
    ));

    // todo announce on gossip
    // todo pending sweeps
    // todo reconnect to peers

    // Set up a oneshot channel to handle shutdown signal
    let (tx, rx) = oneshot::channel();

    // Spawn a task to listen for shutdown signals
    let l = logger.clone();
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

    // Exit if either CLI polling exits or the background processor exits (which shouldn't happen
    // unless we fail to write to the filesystem).
    #[allow(unused_assignments)]
    let mut bg_res = Ok(Ok(()));
    tokio::select! {
        bg_exit = &mut background_processor => {
            bg_res = bg_exit;
        },
        _ = rx => {
            log_info!(logger, "Shutting down...");
        },
    }

    // Disconnect our peers and stop accepting new connections. This ensures we don't continue
    // updating our channel data after we've stopped the background processor.
    stop_listen_connect.store(true, Ordering::Release);
    peer_manager.disconnect_all_peers();

    if let Err(e) = bg_res {
        let persist_res = persister.write(
            persist::CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            persist::CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            persist::CHANNEL_MANAGER_PERSISTENCE_KEY,
            &channel_manager.encode(),
        );

        if let Err(persist_res) = persist_res {
            log_error!(
                logger,
                "Last-ditch ChannelManager persistence result: {:?}",
                persist_res
            );
            panic!(
                "ERR: background processing stopped with result {:?}, exiting.\n\
			Last-ditch ChannelManager persistence result {:?}",
                e, persist_res
            );
        }
    }

    // Stop the background processor.
    if !bp_exit.is_closed() {
        bp_exit.send(())?;
        background_processor.await??;
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeysFile {
    pub seed: [u8; 32],
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
                    let mut seed = [0; 32];
                    OsRng.fill_bytes(&mut seed);
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
