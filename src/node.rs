use crate::chain::TxBroadcaster;
use crate::config::Config;
use crate::events::EventHandler;
use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models;
use crate::models::channel_closure::ChannelClosure;
use crate::models::channel_open_param::ChannelOpenParam;
use crate::onchain::OnChainWallet;
use crate::KeysFile;
use anyhow::anyhow;
use bitcoin::bip32::ExtendedPrivKey;
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::{All, PublicKey, Secp256k1};
use bitcoin::{BlockHash, Network, OutPoint};
use bitcoincore_rpc::{Auth, RpcApi};
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus, Filter, Watch};
use lightning::events::Event;
use lightning::ln::channelmanager::{
    ChainParameters, ChannelManager as LdkChannelManager, ChannelManagerReadArgs,
};
use lightning::ln::peer_handler::{
    IgnoringMessageHandler, MessageHandler, PeerManager as LdkPeerManager,
};
use lightning::ln::ChannelId;
use lightning::onion_message::messenger::DefaultMessageRouter;
use lightning::routing::gossip;
use lightning::routing::gossip::P2PGossipSync;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::{EntropySource, InMemorySigner};
use lightning::util::config::UserConfig;
use lightning::util::logger::Logger;
use lightning::util::persist;
use lightning::util::persist::{KVStore, MonitorUpdatingPersister};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::{log_debug, log_error, log_info};
use lightning_background_processor::{process_events_async, GossipSync};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::{init, poll, SpvClient, UnboundedCache};
use lightning_invoice::utils::{
    create_invoice_from_channelmanager, create_invoice_from_channelmanager_with_description_hash,
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription};
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio::time::{sleep, Instant};

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
    Arc<DefaultMessageRouter<Arc<NetworkGraph>, Arc<RldLogger>>>,
    Arc<ChannelManager>,
    IgnoringMessageHandler,
>;

pub(crate) type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<RldLogger>>;

pub(crate) type Router = DefaultRouter<
    Arc<NetworkGraph>,
    Arc<RldLogger>,
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

#[derive(Clone)]
pub struct Node {
    pub(crate) peer_manager: Arc<PeerManager>,
    pub(crate) keys_manager: Arc<KeysManager>,
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) chain_monitor: Arc<ChainMonitor>,
    pub(crate) fee_estimator: Arc<RldFeeEstimator>,
    pub(crate) network: Network,
    pub(crate) persister: Arc<FilesystemStore>,
    pub(crate) wallet: Arc<OnChainWallet>,
    pub(crate) logger: Arc<RldLogger>,
    pub(crate) secp: Secp256k1<All>,
    pub(crate) db_pool: Pool<ConnectionManager<PgConnection>>,
    stop_listen_connect: Arc<AtomicBool>,
    background_processor: tokio::sync::watch::Receiver<Result<(), std::io::Error>>,
    bp_exit: Arc<tokio::sync::watch::Sender<()>>,
}

impl Node {
    pub async fn new(
        config: &Config,
        db_pool: Pool<ConnectionManager<PgConnection>>,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<Node> {
        // Create the datadir if it doesn't exist
        let path = PathBuf::from(&config.data_dir);
        fs::create_dir_all(path.clone())?;

        let logger = Arc::new(RldLogger::default());

        let keys = KeysFile::read(&path.join("keys.json"), config.network(), &logger)?;

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

        let seed = keys.seed.to_seed_normalized("");
        let xpriv = ExtendedPrivKey::new_master(network, &seed)?;

        let wallet = Arc::new(OnChainWallet::new(
            xpriv,
            network,
            &path.clone().join("bdk"),
            fee_estimator.clone(),
            stop.clone(),
            logger.clone(),
        )?);

        wallet.start();

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
        let (channel_manager_block_hash, channel_manager) = {
            if let Ok(mut f) = fs::File::open(ldk_data_dir.join("manager")) {
                let mut channel_monitor_mut_references = Vec::new();
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
                let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
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

        // Step 18: Handle LDK Events
        let event_handler = EventHandler {
            channel_manager: channel_manager.clone(),
            fee_estimator: fee_estimator.clone(),
            wallet: wallet.clone(),
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
        let bytes = config
            .alias
            .as_deref()
            .unwrap_or("Rust Lightning Daemon")
            .as_bytes();
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

        // todo pending sweeps
        // todo reconnect to peers

        let node = Node {
            peer_manager,
            keys_manager,
            channel_manager,
            chain_monitor,
            fee_estimator,
            network,
            persister,
            wallet,
            logger,
            db_pool,
            secp: Secp256k1::new(),
            stop_listen_connect: stop,
            background_processor: bp_rx,
            bp_exit: Arc::new(bp_exit),
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

    pub fn create_invoice(
        &self,
        description: Bolt11InvoiceDescription,
        msats: Option<u64>,
        expiry: Option<u32>,
    ) -> anyhow::Result<Bolt11Invoice> {
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
        models::invoice::Invoice::create(&mut conn, &invoice)?;

        Ok(invoice)
    }

    pub async fn connect_to_peer(
        &self,
        pubkey: PublicKey,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        for (node_pubkey, _) in self.peer_manager.get_peer_node_ids() {
            if node_pubkey == pubkey {
                return Ok(());
            }
        }
        let res = self.do_connect_peer(pubkey, peer_addr).await;
        if res.is_err() {
            log_error!(self.logger, "ERROR: failed to connect to peer");
        }
        res
    }

    pub(crate) async fn do_connect_peer(
        &self,
        pubkey: PublicKey,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        match lightning_net_tokio::connect_outbound(self.peer_manager.clone(), pubkey, peer_addr)
            .await
        {
            Some(connection_closed_future) => {
                let mut connection_closed_future = Box::pin(connection_closed_future);
                loop {
                    tokio::select! {
                        _ = &mut connection_closed_future => return Err(anyhow!("Connection closed")),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {},
                    }
                    if self
                        .peer_manager
                        .get_peer_node_ids()
                        .iter()
                        .any(|(id, _)| *id == pubkey)
                    {
                        return Ok(());
                    }
                }
            }
            None => Err(anyhow!("Connection timed out")),
        }
    }

    async fn await_chan_funding_tx(
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
                    if ChannelOpenParam::find_by_id(&mut conn, user_channel_id as i32)?
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
    ) -> anyhow::Result<(ChannelId, u128)> {
        // save params to db
        let mut conn = self.db_pool.get()?;
        let params = ChannelOpenParam::create(&mut conn, sats_per_vbyte)?;

        let user_channel_id: u128 = params.id.try_into()?;
        match self.channel_manager.create_channel(
            pubkey,
            amount_sat,
            push_msat,
            user_channel_id,
            None,
            None,
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
        timeout: u64,
    ) -> anyhow::Result<OutPoint> {
        let (_, id) = self
            .init_open_channel(pubkey, amount_sat, push_msat, sats_per_vbyte)
            .await?;

        self.await_chan_funding_tx(id, &pubkey, timeout).await
    }
}