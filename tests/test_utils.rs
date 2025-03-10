use bitcoin::bip32::Xpriv;
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::{Address, Amount, Network};
use corepc_node::tempfile::tempdir;
use corepc_node::{get_available_port, Node as BitcoinD};
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use diesel_migrations::MigrationHarness;
use lazy_static::lazy_static;
use lightning::util::ser::Writeable;
use lnd::tonic_lnd::lnrpc::{
    GetInfoRequest, NewAddressRequest, OpenChannelRequest, WalletBalanceRequest,
};
use lnd::{Lnd, LndConf};
use rld::config::Config;
use rld::logger::RldLogger;
use rld::models::MIGRATIONS;
use rld::node::{Node, PubkeyConnectionInfo};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::ConnectOptions;
use std::env;
use std::net::{SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

lazy_static! {
    pub static ref BITCOIND: BitcoinD = {
        let bitcoind_exe = env::var("BITCOIND_EXE")
            .ok()
            .or_else(|| corepc_node::downloaded_exe_path().ok())
            .expect(
                "you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
            );
        let mut conf = corepc_node::Conf::default();
        conf.args.push("-txindex");
        conf.args.push("-rpcworkqueue=100");
        BitcoinD::with_conf(bitcoind_exe, &conf).unwrap()
    };
    pub static ref MINER: Mutex<()> = Mutex::new(());
}

pub static PREMINE: OnceCell<()> = OnceCell::const_new();

pub async fn generate_blocks_and_wait(num: usize) {
    let _miner = MINER.lock().await;
    generate_blocks(num);
}

pub fn generate_blocks(num: usize) {
    let address = BITCOIND.client.new_address().unwrap();
    let _block_hashes = BITCOIND.client.generate_to_address(num, &address).unwrap();
}

/// Creates a test database using an existing Postgres server
async fn create_test_db_from_url() -> anyhow::Result<String> {
    // Generate a unique name for this test database
    let mut seed = [0; 8];
    OsRng.fill_bytes(&mut seed);
    let test_db_name = format!("rld_test_{}", seed.to_lower_hex_string());

    // Parse the original connection string
    let db_url = env::var("DATABASE_URL")
        .expect("missing DATABASE_URL")
        .to_string();
    let pg_options = PgConnectOptions::from_str(&db_url)?;

    // Connect to the default database to create our test database
    let pg_options_default = pg_options
        .clone()
        .database("postgres")
        .log_statements(log::LevelFilter::Debug);

    let default_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pg_options_default)
        .await?;

    // Create the test database
    sqlx::query(&format!("CREATE DATABASE {}", test_db_name))
        .execute(&default_pool)
        .await?;

    // Close the connection to the default database
    default_pool.close().await;

    // We need to parse the original URL to extract components
    // since PgConnectOptions doesn't expose all parts we need
    let url = url::Url::parse(&db_url)?;

    // Extract password from the original URL
    let password = url.password().unwrap_or("");

    // Create connection string for the new test database
    let test_db_url = format!(
        "postgres://{}:{}@{}:{}/{}",
        pg_options.get_username(),
        password,
        pg_options.get_host(),
        pg_options.get_port(),
        test_db_name
    );

    log::info!("Created test database: {test_db_url}");

    Ok(test_db_url)
}

pub async fn create_rld() -> Node {
    dotenv::dotenv().ok();
    PREMINE
        .get_or_init(|| async {
            generate_blocks_and_wait(101).await;
        })
        .await;

    let logger = Arc::new(RldLogger::default());

    let mut seed = [0; 32];
    OsRng.fill_bytes(&mut seed);
    let network = Network::Regtest;
    let xpriv = Xpriv::new_master(network, &seed).unwrap();

    let config = Config {
        data_dir: Some(tempdir().unwrap().into_path().to_str().unwrap().to_string()),
        pg_url: create_test_db_from_url()
            .await
            .expect("failed to create test db"),
        port: get_available_port().unwrap(),
        rpc_port: get_available_port().unwrap(),
        network: "regtest".to_string(),
        bitcoind_port: BITCOIND.params.rpc_socket.port(),
        bitcoind_rpc_user: BITCOIND.params.get_cookie_values().unwrap().unwrap().user,
        bitcoind_rpc_password: BITCOIND
            .params
            .get_cookie_values()
            .unwrap()
            .unwrap()
            .password,
        ..Default::default()
    };

    // DB management
    let manager = ConnectionManager::<PgConnection>::new(&config.pg_url);
    let db_pool = Pool::builder()
        .max_size(16)
        .test_on_check_out(true)
        .build(manager)
        .expect("Could not build connection pool");

    // run migrations
    let mut connection = db_pool.get().unwrap();
    connection
        .run_pending_migrations(MIGRATIONS)
        .expect("migrations could not run");

    Node::new(
        config,
        xpriv,
        db_pool,
        logger,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap()
}

pub async fn create_lnd() -> Lnd {
    PREMINE
        .get_or_init(|| async {
            generate_blocks_and_wait(101).await;
        })
        .await;

    let lnd_exe = env::var("LND_EXE")
        .ok()
        .or_else(lnd::downloaded_exe_path)
        .expect("you need to provide env var LND_EXE or specify an lnd version feature");
    let mut config = LndConf::default();
    config.view_stdout = false;
    let rpc_cookie = BITCOIND.params.cookie_file.to_str().unwrap().to_string();
    let rpc_host = BITCOIND.params.rpc_socket.to_string();
    let mut lnd = Lnd::with_conf(lnd_exe, &config, rpc_cookie, rpc_host, &BITCOIND)
        .await
        .unwrap();
    let lightning = lnd.client.lightning();

    let lnd_address = lightning
        .new_address(NewAddressRequest {
            r#type: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    let lnd_address = Address::from_str(&lnd_address.into_inner().address).unwrap();

    let amt = 100_000_000;
    BITCOIND
        .client
        .send_to_address(&lnd_address.assume_checked(), Amount::from_sat(amt))
        .unwrap();

    generate_blocks_and_wait(1).await;

    // wait for lnd to sync
    for _ in 0..240 {
        let balance = lightning
            .wallet_balance(WalletBalanceRequest {})
            .await
            .unwrap();
        let balance = balance.into_inner();
        if balance.confirmed_balance >= amt as i64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    wait_for_lnd_sync(&mut lnd).await;

    lnd
}

pub async fn wait_for_lnd_sync(lnd: &mut Lnd) {
    let lightning = lnd.client.lightning();
    for _ in 0..240 {
        let info = lightning.get_info(GetInfoRequest {}).await.unwrap();
        let info = info.into_inner();
        if info.synced_to_chain {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    panic!("lnd did not sync");
}

pub async fn get_lnd_connection_info(lnd: &mut Lnd) -> PubkeyConnectionInfo {
    let lightning = lnd.client.lightning();
    let info = lightning.get_info(GetInfoRequest {}).await.unwrap();
    let info = info.into_inner();

    let connection_string = format!(
        "{}@{}",
        info.identity_pubkey,
        lnd.listen_url.as_deref().unwrap()
    );

    PubkeyConnectionInfo::new(&connection_string).unwrap()
}

pub async fn fund_rld(rld: &Node) {
    let start = rld.wallet.balance();
    let address = rld.wallet.get_new_address().unwrap();

    let amt = Amount::from_sat(100_000_000);
    BITCOIND
        .client
        .send_to_address(&address.address, amt)
        .unwrap();

    generate_blocks_and_wait(6).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let balance = rld.wallet.balance();
    assert_eq!(balance.confirmed, amt + start.confirmed);
}

pub async fn open_channel_from_rld(node: &Node, lnd: &mut Lnd) {
    // get some coins in the wallet
    fund_rld(node).await;
    // make sure lnd is synced
    wait_for_lnd_sync(lnd).await;

    let connection_info = get_lnd_connection_info(lnd).await;
    node.connect_to_peer(connection_info.pubkey, connection_info.socket_addr)
        .await
        .unwrap();

    // wait for stable connection
    tokio::time::sleep(Duration::from_millis(250)).await;

    let _ = node
        .open_channel_with_timeout(connection_info.pubkey, 1_000_000, 0, None, false, 30)
        .await
        .unwrap();

    let blocks = 6;
    generate_blocks_and_wait(blocks).await;

    // wait for channel to be usable
    tokio::time::sleep(Duration::from_secs(10)).await;
    wait_for_lnd_sync(lnd).await;

    let chans = node.channel_manager.list_channels();
    assert_eq!(chans.len(), 1);
    assert!(chans[0].is_outbound);
    assert!(chans[0].confirmations_required.unwrap() <= blocks as u32);
    println!("{}", chans[0].confirmations.unwrap());
    assert!(chans[0].confirmations.unwrap() >= chans[0].confirmations_required.unwrap());
    assert!(chans[0].is_channel_ready);
    assert!(chans[0].is_usable);
    assert!(chans[0]
        .clone()
        .channel_type
        .unwrap()
        .supports_anchors_zero_fee_htlc_tx());
}

pub async fn open_channel_from_lnd(node: &Node, lnd: &mut Lnd) {
    let connection_info = get_lnd_connection_info(lnd).await;
    node.connect_to_peer(connection_info.pubkey, connection_info.socket_addr)
        .await
        .unwrap();

    // wait for stable connection
    tokio::time::sleep(Duration::from_millis(250)).await;

    wait_for_lnd_sync(lnd).await;

    let node_id = node.node_id();
    let lightning = lnd.client.lightning();

    let _ = lightning
        .open_channel_sync(OpenChannelRequest {
            node_pubkey: node_id.encode(),
            local_funding_amount: 1_000_000,
            private: true,
            ..Default::default()
        })
        .await
        .unwrap();

    generate_blocks_and_wait(6).await;

    // wait for channel to be usable
    tokio::time::sleep(Duration::from_secs(2)).await;
    wait_for_lnd_sync(lnd).await;

    let chans = node.channel_manager.list_channels();
    assert_eq!(chans.len(), 1);
    assert!(chans[0].is_usable);
    assert!(chans[0]
        .clone()
        .channel_type
        .unwrap()
        .supports_anchors_zero_fee_htlc_tx());
}

pub async fn open_channel(node1: &Node, node2: &Node) {
    // get some coins in the wallet
    fund_rld(node1).await;
    fund_rld(node2).await;

    let pk = node2.node_id();
    let connection_info =
        SocketAddr::from_str(&format!("127.0.0.1:{}", node2.config.port)).unwrap();
    node1
        .connect_to_peer(
            pk,
            connection_info.to_socket_addrs().unwrap().next().unwrap(),
        )
        .await
        .unwrap();

    // wait for stable connection
    tokio::time::sleep(Duration::from_millis(250)).await;

    let _ = node1
        .open_channel_with_timeout(pk, 1_000_000, 0, None, false, 30)
        .await
        .unwrap();

    generate_blocks_and_wait(6).await;

    // wait for channel to be usable
    tokio::time::sleep(Duration::from_secs(2)).await;

    let chans = node1.channel_manager.list_channels();
    assert_eq!(chans.len(), 1);
    assert!(chans[0].is_usable);
    assert!(chans[0]
        .clone()
        .channel_type
        .unwrap()
        .supports_anchors_zero_fee_htlc_tx());

    let chans = node2.channel_manager.list_channels();
    assert_eq!(chans.len(), 1);
    assert!(chans[0].is_usable);
    assert!(chans[0]
        .clone()
        .channel_type
        .unwrap()
        .supports_anchors_zero_fee_htlc_tx());
}
