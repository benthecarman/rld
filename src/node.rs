use crate::chain::TxBroadcaster;
use crate::config::Config;
use crate::events::EventHandler;
use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models;
use crate::models::channel::Channel;
use crate::models::channel_closure::ChannelClosure;
use crate::models::connect_info::ConnectionInfo;
use crate::models::payment::{Payment, PaymentStatus};
use crate::models::receive::Receive;
use crate::models::CreatedInvoice;
use crate::onchain::OnChainWallet;
use anyhow::anyhow;
use bitcoin::bip32::ExtendedPrivKey;
use bitcoin::hashes::{sha256::Hash as Sha256Hash, Hash};
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::{All, PublicKey, Secp256k1, ThirtyTwoByteHash};
use bitcoin::{BlockHash, Network, OutPoint};
use bitcoincore_rpc::{Auth, RpcApi};
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::{Connection, PgConnection};
use lightning::blinded_path::EmptyNodeIdLookUp;
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus, Filter, Watch};
use lightning::events::bump_transaction::{BumpTransactionEventHandler, Wallet};
use lightning::events::Event;
use lightning::ln::channelmanager::{
    ChainParameters, ChannelDetails, ChannelManager as LdkChannelManager, ChannelManagerReadArgs,
    PaymentId, RecipientOnionFields, Retry,
};
use lightning::ln::peer_handler::{
    IgnoringMessageHandler, MessageHandler, PeerManager as LdkPeerManager,
};
use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::onion_message::messenger::DefaultMessageRouter;
use lightning::routing::gossip;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::router::{DefaultRouter, PaymentParameters, RouteParameters};
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::{EntropySource, InMemorySigner, NodeSigner, Recipient};
use lightning::util::config::{
    ChannelConfig, ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig,
};
use lightning::util::logger::Logger;
use lightning::util::persist;
use lightning::util::persist::{KVStore, MonitorUpdatingPersister};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::{log_debug, log_error, log_info};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::{init, poll, SpvClient, UnboundedCache};
use lightning_invoice::payment::{
    payment_parameters_from_invoice, payment_parameters_from_zero_amount_invoice,
};
use lightning_invoice::utils::{
    create_invoice_from_channelmanager, create_invoice_from_channelmanager_with_description_hash,
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio::time::{sleep, Instant};
use tonic::Status;

const DEFAULT_PAYMENT_TIMEOUT: u64 = 30;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<RldLogger>>;

pub(crate) type GossipVerifier = lightning_block_sync::gossip::GossipVerifier<
    lightning_block_sync::gossip::TokioSpawner,
    Arc<lightning_block_sync::rpc::RpcClient>,
    Arc<RldLogger>,
>;

pub(crate) type PeerManager = LdkPeerManager<
    SocketDescriptor,
    Arc<ChannelManager>,
    Arc<P2PGossipSync<Arc<NetworkGraph>, GossipVerifier, Arc<RldLogger>>>,
    Arc<OnionMessenger>,
    Arc<RldLogger>,
    IgnoringMessageHandler,
    Arc<KeysManager>,
>;

type OnionMessenger = lightning::onion_message::messenger::OnionMessenger<
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<RldLogger>,
    Arc<EmptyNodeIdLookUp>,
    Arc<DefaultMessageRouter<Arc<NetworkGraph>, Arc<RldLogger>, Arc<KeysManager>>>,
    Arc<ChannelManager>,
    IgnoringMessageHandler,
>;

pub(crate) type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<RldLogger>>;

pub(crate) type Router = DefaultRouter<
    Arc<NetworkGraph>,
    Arc<RldLogger>,
    Arc<KeysManager>,
    Arc<RwLock<Scorer>>,
    ProbabilisticScoringFeeParameters,
    Scorer,
>;

pub(crate) type ChannelManager = LdkChannelManager<
    Arc<ChainMonitor>,
    Arc<TxBroadcaster>,
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<RldFeeEstimator>,
    Arc<Router>,
    Arc<RldLogger>,
>;

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

pub(crate) type BumpTxEventHandler = BumpTransactionEventHandler<
    Arc<TxBroadcaster>,
    Arc<Wallet<Arc<OnChainWallet>, Arc<RldLogger>>>,
    Arc<KeysManager>,
    Arc<RldLogger>,
>;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PubkeyConnectionInfo {
    pub pubkey: PublicKey,
    pub socket_addr: SocketAddr,
    pub original_connection_string: String,
}

impl PubkeyConnectionInfo {
    pub fn new(connection: &str) -> anyhow::Result<Self> {
        if connection.is_empty() {
            return Err(anyhow!("connection is empty"));
        };
        let connection = connection.to_lowercase();
        let (pubkey, peer_addr_str) = parse_peer_info(&connection)?;
        Ok(Self {
            pubkey,
            socket_addr: SocketAddr::from_str(&peer_addr_str)
                .map_err(|_| anyhow!("invalid peer address"))?,
            original_connection_string: connection,
        })
    }
}

pub(crate) fn parse_peer_info(
    peer_pubkey_and_ip_addr: &str,
) -> anyhow::Result<(PublicKey, String)> {
    let (pubkey, peer_addr_str) = split_peer_connection_string(peer_pubkey_and_ip_addr)?;

    let peer_addr_str_with_port = if peer_addr_str.contains(':') {
        peer_addr_str
    } else {
        format!("{peer_addr_str}:9735")
    };

    Ok((pubkey, peer_addr_str_with_port))
}

pub(crate) fn split_peer_connection_string(
    peer_pubkey_and_ip_addr: &str,
) -> anyhow::Result<(PublicKey, String)> {
    let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split('@');
    let pubkey = pubkey_and_addr
        .next()
        .ok_or_else(|| anyhow!("pubkey is empty"))?;
    let peer_addr_str = pubkey_and_addr
        .next()
        .ok_or_else(|| anyhow!("peer address is empty"))?;
    let pubkey = PublicKey::from_str(pubkey).map_err(|_| anyhow!("invalid pubkey"))?;
    Ok((pubkey, peer_addr_str.to_string()))
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Balance {
    pub confirmed: u64,
    pub unconfirmed: u64,
    pub lightning: u64,
    pub force_close: u64,
}

impl Balance {
    pub fn on_chain(&self) -> u64 {
        self.confirmed + self.unconfirmed
    }
}

#[derive(Clone)]
pub struct Node {
    pub(crate) config: Config,
    pub(crate) peer_manager: Arc<PeerManager>,
    pub(crate) keys_manager: Arc<KeysManager>,
    pub channel_manager: Arc<ChannelManager>,
    pub(crate) chain_monitor: Arc<ChainMonitor>,
    pub(crate) network_graph: Arc<NetworkGraph>,
    pub(crate) router: Arc<Router>,
    pub network: Network,
    pub(crate) persister: Arc<FilesystemStore>,
    pub wallet: Arc<OnChainWallet>,
    pub(crate) bitcoind: Arc<bitcoincore_rpc::Client>,
    pub logger: Arc<RldLogger>,
    pub(crate) secp: Secp256k1<All>,
    pub(crate) db_pool: Pool<ConnectionManager<PgConnection>>,
    pub(crate) stop_listen_connect: Arc<AtomicBool>,
    background_processor: tokio::sync::watch::Receiver<Result<(), std::io::Error>>,
    bp_exit: Arc<tokio::sync::watch::Sender<()>>,

    // broadcast channels
    pub(crate) invoice_broadcast: broadcast::Sender<Receive>,
    #[allow(unused)]
    pub(crate) invoice_rx: Arc<broadcast::Receiver<Receive>>,
}

impl Node {
    pub async fn new(
        config: Config,
        xpriv: ExtendedPrivKey,
        db_pool: Pool<ConnectionManager<PgConnection>>,
        logger: Arc<RldLogger>,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<Node> {
        let path = PathBuf::from(&config.data_dir);

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

        let wallet = Arc::new(OnChainWallet::new(
            xpriv,
            network,
            &path.clone().join("bdk"),
            fee_estimator.clone(),
            stop.clone(),
            logger.clone(),
        )?);

        wallet.start();

        let broadcaster = Arc::new(TxBroadcaster::new(
            bitcoind.clone(),
            wallet.clone(),
            logger.clone(),
        ));

        let keys_manager = Arc::new(KeysManager::new(
            xpriv,
            network,
            wallet.clone(),
            logger.clone(),
        )?);

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

        let mut channel_monitors =
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
            keys_manager.clone(),
            scorer.clone(),
            scoring_fee_params,
        ));

        // Step 11: Initialize the ChannelManager
        let mut restarting_node = true;
        let (channel_manager_block_hash, channel_manager) = {
            if let Ok(mut f) = fs::File::open(ldk_data_dir.join("manager")) {
                let mut channel_monitor_mut_references = Vec::with_capacity(channel_monitors.len());
                for (_, channel_monitor) in channel_monitors.iter_mut() {
                    channel_monitor_mut_references.push(channel_monitor);
                }
                let read_args = ChannelManagerReadArgs::new(
                    keys_manager.clone(),
                    keys_manager.clone(),
                    keys_manager.clone(),
                    fee_estimator.clone(),
                    chain_monitor.clone(),
                    broadcaster.clone(),
                    router.clone(),
                    logger.clone(),
                    default_user_config(),
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
                let polled_best_block_hash = polled_best_block.block_hash;
                let chain_params = ChainParameters {
                    network,
                    best_block: polled_best_block,
                };
                let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
                let fresh_channel_manager = ChannelManager::new(
                    fee_estimator.clone(),
                    chain_monitor.clone(),
                    broadcaster.clone(),
                    router.clone(),
                    logger.clone(),
                    keys_manager.clone(),
                    keys_manager.clone(),
                    keys_manager.clone(),
                    default_user_config(),
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
                channel_manager_block_hash,
                &channel_manager as &(dyn lightning::chain::Listen + Send + Sync),
            )];

            for (block_hash, channel_monitor) in channel_monitors.drain(..) {
                let outpoint = channel_monitor.get_funding_txo().0;
                chain_listener_channel_monitors.push((
                    block_hash,
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
            Arc::new(EmptyNodeIdLookUp {}),
            Arc::new(DefaultMessageRouter::new(
                network_graph.clone(),
                keys_manager.clone(),
            )),
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

        // Install a GossipVerifier in the P2PGossipSync
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
        let stop_listen = stop.clone();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
                .await
                .expect(
                    "Failed to bind to listen port - is something else already listening on it?",
                );
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
            let mut spv_client =
                SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
            loop {
                spv_client.poll_best_tip().await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        // create a BumpTransactionEventHandler
        let bump_tx_event_handler = Arc::new(BumpTransactionEventHandler::new(
            broadcaster.clone(),
            Arc::new(Wallet::new(wallet.clone(), logger.clone())),
            keys_manager.clone(),
            logger.clone(),
        ));

        let (invoice_broadcast, invoice_rx) = broadcast::channel(16);

        // Step 18: Handle LDK Events
        let event_handler = EventHandler {
            channel_manager: channel_manager.clone(),
            peer_manager: peer_manager.clone(),
            fee_estimator,
            wallet: wallet.clone(),
            keys_manager: keys_manager.clone(),
            bump_tx_event_handler,
            db_pool: db_pool.clone(),
            logger: logger.clone(),
            invoice_broadcast: invoice_broadcast.clone(),
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
        let (bp_tx, bp_rx) = tokio::sync::watch::channel(Ok(()));
        let bp_persister = persister.clone();
        let bp_peer_manager = peer_manager.clone();
        let bp_channel_manager = channel_manager.clone();
        let bp_chain_monitor = chain_monitor.clone();
        let bp_gossip_sync = gossip_sync.clone();
        let bp_logger = logger.clone();
        tokio::spawn(async move {
            let res = process_events_async(
                bp_persister,
                event_handler_func,
                bp_chain_monitor,
                bp_channel_manager,
                GossipSync::p2p(bp_gossip_sync),
                bp_peer_manager,
                bp_logger.clone(),
                Some(scorer),
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
            )
            .await;

            if let Err(e) = bp_tx.send(res) {
                log_error!(bp_logger, "Failed to send background processor result: {e}");
            }
        });

        // Regularly broadcast our node_announcement. This is only required (or possible) if we have
        // some public channels.
        let peer_man = Arc::clone(&peer_manager);
        let chan_man = Arc::clone(&channel_manager);
        let mut alias: [u8; 32] = [0; 32];
        let bytes = config.alias().as_bytes();
        let len = bytes.len().min(alias.len());
        alias[..len].copy_from_slice(&bytes[..len]);
        let list_address = config.list_address();
        tokio::spawn(async move {
            let list_addresses = list_address.map(|addr| vec![addr]).unwrap_or_default();
            // First wait a minute until we have some peers and maybe have opened a channel.
            tokio::time::sleep(Duration::from_secs(60)).await;
            // Then, update our announcement once an hour to keep it fresh but avoid unnecessary churn
            // in the global gossip network.
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                // Don't bother trying to announce if we don't have any public channels, though our
                // peers should drop such an announcement anyway. Note that announcement may not
                // propagate until we have a channel with 6+ confirmations.
                if chan_man.list_channels().iter().any(|chan| chan.is_public) {
                    peer_man.broadcast_node_announcement([0; 3], alias, list_addresses.clone());
                }
            }
        });

        // Reconnect to peers every 120 seconds
        let reconnect_graph = network_graph.clone();
        let reconnect_peer_manager = peer_manager.clone();
        let reconnect_channel_manager = channel_manager.clone();
        let reconnect_db_pool = db_pool.clone();
        let reconnect_logger = logger.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(120));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let channels = reconnect_channel_manager.list_channels();
                let missing_peers = channels
                    .iter()
                    .filter(|c| !c.is_usable)
                    .map(|c| c.counterparty.node_id)
                    .collect::<HashSet<_>>();

                let current_peers = reconnect_peer_manager.list_peers();

                for peer in missing_peers {
                    if current_peers.iter().any(|p| p.counterparty_node_id == peer) {
                        continue;
                    }
                    log_info!(reconnect_logger, "Reconnecting to peer: {peer}");

                    let addresses = {
                        reconnect_graph
                            .read_only()
                            .node(&peer.into())
                            .and_then(|n| n.announcement_info.clone())
                            .and_then(|a| a.announcement_message)
                            .map(|a| a.contents.addresses)
                            .map(|a| {
                                a.iter()
                                    .flat_map(|a| a.to_socket_addrs().unwrap_or_default())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    };

                    let mut connected = false;
                    for addr in addresses {
                        log_info!(reconnect_logger, "Connecting to peer: {peer} at: {addr}");
                        match Self::do_connect_peer(reconnect_peer_manager.clone(), peer, addr)
                            .await
                        {
                            Ok(_) => {
                                connected = true;
                                break;
                            }
                            Err(e) => {
                                log_error!(reconnect_logger, "Error connecting to peer: {e}")
                            }
                        }
                    }

                    if !connected {
                        let Ok(mut conn) = reconnect_db_pool.get() else {
                            log_error!(
                                reconnect_logger,
                                "Could not get database connection for reconnect thread"
                            );
                            continue;
                        };
                        let info = ConnectionInfo::find_by_node_id(&mut conn, peer.encode())
                            .unwrap_or_default();

                        for addr in info {
                            log_info!(reconnect_logger, "Connecting to peer: {peer} at: {addr}");
                            if let Ok(addr) = SocketAddr::from_str(&addr) {
                                match Self::do_connect_peer(
                                    reconnect_peer_manager.clone(),
                                    peer,
                                    addr,
                                )
                                .await
                                {
                                    Ok(_) => break,
                                    Err(e) => {
                                        log_error!(
                                            reconnect_logger,
                                            "Error connecting to peer: {e}"
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        let node = Node {
            config,
            peer_manager,
            keys_manager,
            channel_manager,
            chain_monitor,
            network_graph,
            router,
            network,
            persister,
            wallet,
            bitcoind,
            logger,
            db_pool,
            secp: Secp256k1::new(),
            stop_listen_connect: stop,
            background_processor: bp_rx,
            bp_exit: Arc::new(bp_exit),
            invoice_broadcast,
            invoice_rx: Arc::new(invoice_rx),
        };

        Ok(node)
    }

    pub async fn stop(&self) -> anyhow::Result<()> {
        log_info!(self.logger, "Shutting down");
        // Disconnect our peers and stop accepting new connections. This ensures we don't continue
        // updating our channel data after we've stopped the background processor.
        self.stop_listen_connect.store(true, Ordering::Release);
        self.peer_manager.disconnect_all_peers();

        let persist_res = self.persister.write(
            persist::CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            persist::CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            persist::CHANNEL_MANAGER_PERSISTENCE_KEY,
            &self.channel_manager.encode(),
        );

        if let Err(persist_res) = persist_res {
            log_error!(
                self.logger,
                "Last-ditch ChannelManager persistence result: {persist_res:?}"
            );
        }

        // Stop the background processor.
        if !self.bp_exit.is_closed() {
            self.bp_exit.send(())?;
            let mut bp = self.background_processor.clone();
            bp.changed().await?;
            let res = bp.borrow();
            match res.as_ref() {
                Ok(()) => (),
                Err(e) => {
                    log_error!(self.logger, "Background processor error: {e}");
                    return Err(anyhow!("Background processor error: {e}"));
                }
            }
        }

        Ok(())
    }

    pub fn node_id(&self) -> PublicKey {
        self.keys_manager
            .get_node_id(Recipient::Node)
            .expect("invalid node id")
    }

    pub fn get_balance(&self) -> Balance {
        let wallet_balance = self.wallet.balance();

        let lightning_msats: u64 = self
            .channel_manager
            .list_channels()
            .iter()
            .map(|c| c.balance_msat)
            .sum();

        // get the amount in limbo from force closes
        let channels = self.channel_manager.list_channels();
        let ignored_channels: Vec<&ChannelDetails> = channels.iter().collect();
        let force_close: u64 = self
            .chain_monitor
            .get_claimable_balances(&ignored_channels)
            .iter()
            .map(|bal| bal.claimable_amount_satoshis())
            .sum();

        Balance {
            confirmed: wallet_balance.confirmed,
            unconfirmed: wallet_balance.untrusted_pending + wallet_balance.trusted_pending,
            lightning: lightning_msats / 1_000,
            force_close,
        }
    }

    pub fn create_invoice(
        &self,
        description: Bolt11InvoiceDescription,
        msats: Option<u64>,
        expiry: Option<u32>,
    ) -> anyhow::Result<CreatedInvoice> {
        let result = match description {
            Bolt11InvoiceDescription::Direct(desc) => create_invoice_from_channelmanager(
                &self.channel_manager,
                self.keys_manager.clone(),
                self.logger.clone(),
                self.network.into(),
                msats,
                desc.to_string(),
                expiry.unwrap_or(86_400),
                None,
            ),
            Bolt11InvoiceDescription::Hash(hash) => {
                create_invoice_from_channelmanager_with_description_hash(
                    &self.channel_manager,
                    self.keys_manager.clone(),
                    self.logger.clone(),
                    self.network.into(),
                    msats,
                    hash.clone(),
                    expiry.unwrap_or(86_400),
                    None,
                )
            }
        };

        let invoice = result.map_err(|e| anyhow!("Failed to create invoice: {e}"))?;

        let mut conn = self.db_pool.get()?;
        let inv = Receive::create(&mut conn, &invoice)?;

        let id = inv.id;
        self.invoice_broadcast.send(inv)?;

        Ok(CreatedInvoice {
            id,
            bolt11: invoice,
        })
    }

    pub(crate) async fn await_payment(
        &self,
        payment_id: PaymentId,
        payment_hash: PaymentHash,
        timeout: u64,
    ) -> anyhow::Result<Payment> {
        let start = Instant::now();
        loop {
            if start.elapsed().as_secs() > timeout {
                // stop retrying after timeout, this should help prevent
                // payments completing unexpectedly after the timeout
                self.channel_manager.abandon_payment(payment_id);
                return Err(anyhow::anyhow!("Payment timed out"));
            }

            let payment_info = {
                let mut conn = self.db_pool.get()?;
                Payment::find(&mut conn, payment_hash.0)?
            };

            if let Some(info) = payment_info {
                match info.status() {
                    PaymentStatus::Completed => return Ok(info),
                    PaymentStatus::Failed => return Err(anyhow!("Payment failed")),
                    PaymentStatus::Pending => {}
                }
            }

            sleep(Duration::from_millis(250)).await;
        }
    }

    pub async fn init_pay_invoice(
        &self,
        invoice: Bolt11Invoice,
        amount_msats: Option<u64>,
    ) -> anyhow::Result<(PaymentId, PaymentHash)> {
        if invoice.network() != self.network {
            anyhow::bail!("Network mismatch");
        }

        let (hash, onion, route_params) = match invoice.amount_milli_satoshis() {
            Some(_) => payment_parameters_from_invoice(&invoice).expect("already checked"),
            None => match amount_msats {
                Some(msats) => payment_parameters_from_zero_amount_invoice(&invoice, msats)
                    .expect("already checked"),
                None => return Err(anyhow!("No amount provided")),
            },
        };

        let amount_msats = route_params.final_value_msat;

        let payment_id = PaymentId(hash.0);
        self.channel_manager
            .send_payment(
                hash,
                onion,
                payment_id,
                route_params,
                Retry::Timeout(Duration::from_secs(30)),
            )
            .map_err(|e| Status::internal(format!("{e:?}")))?;

        let mut conn = self.db_pool.get()?;
        let pk = invoice.get_payee_pub_key();
        Payment::create(
            &mut conn,
            hash,
            amount_msats as i64,
            Some(pk),
            Some(invoice),
            None,
        )?;

        Ok((payment_id, hash))
    }

    pub async fn pay_invoice_with_timeout(
        &self,
        invoice: Bolt11Invoice,
        amt_sats: Option<u64>,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<Payment> {
        // initiate payment
        let (payment_id, payment_hash) = self.init_pay_invoice(invoice, amt_sats).await?;
        let timeout: u64 = timeout_secs.unwrap_or(DEFAULT_PAYMENT_TIMEOUT);

        self.await_payment(payment_id, payment_hash, timeout).await
    }

    pub async fn init_keysend(
        &self,
        node_id: PublicKey,
        amount_msats: u64,
        custom_tlvs: Vec<(u64, Vec<u8>)>,
    ) -> anyhow::Result<(PaymentId, PaymentHash)> {
        let payment_secret = PaymentSecret(self.keys_manager.get_secure_random_bytes());
        let preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
        let payment_hash = Sha256Hash::hash(&preimage.0);

        let payment_params = PaymentParameters::for_keysend(node_id, 40, false);
        let route_params: RouteParameters = RouteParameters {
            final_value_msat: amount_msats,
            payment_params,
            max_total_routing_fee_msat: None,
        };

        let recipient_onion = if custom_tlvs.is_empty() {
            RecipientOnionFields::secret_only(payment_secret)
        } else {
            RecipientOnionFields::secret_only(payment_secret)
                .with_custom_tlvs(custom_tlvs)
                .map_err(|_| anyhow::anyhow!("Invalid custom TLVs"))?
        };

        let payment_id = PaymentId(payment_hash.into_32());
        let payment_hash = self
            .channel_manager
            .send_spontaneous_payment_with_retry(
                Some(preimage),
                recipient_onion,
                payment_id,
                route_params,
                Retry::Timeout(Duration::from_secs(30)),
            )
            .map_err(|e| anyhow::anyhow!("Error sending keysend: {e:?}"))?;

        let mut conn = self.db_pool.get()?;
        Payment::create(
            &mut conn,
            payment_hash,
            amount_msats as i64,
            Some(node_id),
            None,
            None,
        )?;

        Ok((payment_id, payment_hash))
    }

    pub async fn keysend_with_timeout(
        &self,
        node_id: PublicKey,
        amount_msats: u64,
        custom_tlvs: Vec<(u64, Vec<u8>)>,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<Payment> {
        // initiate payment
        let (payment_id, payment_hash) = self
            .init_keysend(node_id, amount_msats, custom_tlvs)
            .await?;
        let timeout: u64 = timeout_secs.unwrap_or(DEFAULT_PAYMENT_TIMEOUT);

        self.await_payment(payment_id, payment_hash, timeout).await
    }

    pub async fn connect_to_peer(
        &self,
        pubkey: PublicKey,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        for peer in self.peer_manager.list_peers() {
            if peer.counterparty_node_id == pubkey {
                return Ok(());
            }
        }

        // save connection info
        let mut conn = self.db_pool.get()?;
        ConnectionInfo::upsert(&mut conn, pubkey.encode(), peer_addr.to_string(), false)?;
        drop(conn);

        let res = Self::do_connect_peer(self.peer_manager.clone(), pubkey, peer_addr).await;
        if res.is_err() {
            log_error!(self.logger, "ERROR: failed to connect to peer");
        }
        res
    }

    pub(crate) async fn do_connect_peer(
        peer_manager: Arc<PeerManager>,
        pubkey: PublicKey,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        match lightning_net_tokio::connect_outbound(peer_manager.clone(), pubkey, peer_addr).await {
            Some(connection_closed_future) => {
                let mut connection_closed_future = Box::pin(connection_closed_future);
                loop {
                    tokio::select! {
                        _ = &mut connection_closed_future => return Err(anyhow!("Connection closed")),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {},
                    }
                    if peer_manager
                        .list_peers()
                        .iter()
                        .any(|peer| peer.counterparty_node_id == pubkey)
                    {
                        return Ok(());
                    }
                }
            }
            None => Err(anyhow!("Connection timed out")),
        }
    }

    pub(crate) async fn await_chan_funding_tx(
        &self,
        user_channel_id: u128,
        pubkey: &PublicKey,
        timeout: u64,
    ) -> anyhow::Result<OutPoint> {
        let start = Instant::now();
        loop {
            if self.stop_listen_connect.load(Ordering::Relaxed) {
                anyhow::bail!("Not running");
            }

            let mut conn = self.db_pool.get()?;
            // We will get a channel closure event if the peer rejects the channel
            // todo return closure reason to user
            if let Ok(Some(_closure)) =
                ChannelClosure::find_by_id(&mut conn, user_channel_id as i32)
            {
                anyhow::bail!("Channel creation failed");
            }

            let channels = self.channel_manager.list_channels_with_counterparty(pubkey);
            let channel = channels
                .iter()
                .find(|c| c.user_channel_id == user_channel_id);

            if let Some(outpoint) = channel.and_then(|c| c.funding_txo) {
                log_info!(self.logger, "Channel funding tx found: {outpoint}");
                log_debug!(self.logger, "Waiting for Channel Pending event");
                loop {
                    // make sure the channel is open
                    if Channel::find_by_id(&mut conn, user_channel_id as i32)?
                        .is_some_and(|p| p.success)
                    {
                        return Ok(outpoint.into_bitcoin_outpoint());
                    }

                    let elapsed = start.elapsed().as_secs();
                    if elapsed > timeout {
                        anyhow::bail!("Channel creation timed out");
                    }

                    if self.stop_listen_connect.load(Ordering::Relaxed) {
                        anyhow::bail!("Not running");
                    }
                    sleep(Duration::from_millis(250)).await;
                }
            }

            let elapsed = start.elapsed().as_secs();
            if elapsed > timeout {
                anyhow::bail!("Channel creation timed out");
            }

            sleep(Duration::from_millis(250)).await;
        }
    }

    pub async fn init_open_channel(
        &self,
        pubkey: PublicKey,
        amount_sat: u64,
        push_msat: u64,
        sats_per_vbyte: Option<i32>,
        private: bool,
    ) -> anyhow::Result<(ChannelId, u128)> {
        // check we're connected to the peer
        if !self
            .peer_manager
            .list_peers()
            .iter()
            .any(|p| p.counterparty_node_id == pubkey)
        {
            return Err(anyhow!("Not connected to peer"));
        }

        // save params to db
        let mut conn = self.db_pool.get()?;
        let params = conn.transaction::<_, anyhow::Error, _>(|conn| {
            // set reconnect flag on connection info
            ConnectionInfo::set_reconnect(conn, pubkey.encode())?;

            let params = Channel::create(
                conn,
                pubkey.encode(),
                sats_per_vbyte,
                push_msat as i64,
                private,
                true,
                amount_sat as i64,
                false,
            )?;
            Ok(params)
        })?;

        let user_channel_id: u128 = params.id.try_into()?;
        let mut config = default_user_config();
        config.channel_handshake_config.announced_channel = !private;
        match self.channel_manager.create_channel(
            pubkey,
            amount_sat,
            push_msat,
            user_channel_id,
            None,
            Some(config),
        ) {
            Ok(channel_id) => {
                log_info!(
                    self.logger,
                    "SUCCESS: channel initiated with peer: {pubkey:?}"
                );
                Ok((channel_id, user_channel_id))
            }
            Err(e) => {
                log_error!(
                    self.logger,
                    "ERROR: failed to open channel to pubkey {pubkey:?}: {e:?}"
                );
                Err(anyhow!(
                    "Failed to open channel to pubkey {pubkey:?}: {e:?}"
                ))
            }
        }
    }

    pub async fn open_channel_with_timeout(
        &self,
        pubkey: PublicKey,
        amount_sat: u64,
        push_msat: u64,
        sats_per_vbyte: Option<i32>,
        private: bool,
        timeout: u64,
    ) -> anyhow::Result<OutPoint> {
        let (_, id) = self
            .init_open_channel(pubkey, amount_sat, push_msat, sats_per_vbyte, private)
            .await?;

        self.await_chan_funding_tx(id, &pubkey, timeout).await
    }
}

fn default_user_config() -> UserConfig {
    UserConfig {
        channel_handshake_config: ChannelHandshakeConfig {
            negotiate_scid_privacy: true,
            negotiate_anchors_zero_fee_htlc_tx: true,
            announced_channel: false,
            ..Default::default()
        },
        channel_handshake_limits: ChannelHandshakeLimits {
            force_announced_channel_preference: false,
            ..Default::default()
        },
        channel_config: ChannelConfig {
            forwarding_fee_proportional_millionths: 10_000,
            accept_underpaying_htlcs: false,
            ..Default::default()
        },
        accept_forwards_to_priv_channels: true,
        accept_inbound_channels: true,
        manually_accept_inbound_channels: true,
        accept_mpp_keysend: true,
        ..Default::default()
    }
}
