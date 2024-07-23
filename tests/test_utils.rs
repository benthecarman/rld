use bitcoin::bip32::ExtendedPrivKey;
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::{Address, Amount, Network};
use bitcoind::bitcoincore_rpc::bitcoincore_rpc_json::AddressType;
use bitcoind::bitcoincore_rpc::RpcApi;
use bitcoind::tempfile::tempdir;
use bitcoind::{get_available_port, BitcoinD};
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
use std::env;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

lazy_static! {
    pub static ref BITCOIND: BitcoinD = {
        let bitcoind_exe = env::var("BITCOIND_EXE")
            .ok()
            .or_else(|| bitcoind::downloaded_exe_path().ok())
            .expect(
                "you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
            );
        let mut conf = bitcoind::Conf::default();
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
    let address = BITCOIND
        .client
        .get_new_address(Some("test"), Some(AddressType::Bech32m))
        .unwrap()
        .assume_checked();
    let _block_hashes = BITCOIND
        .client
        .generate_to_address(num as u64, &address)
        .unwrap();
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
    let xpriv = ExtendedPrivKey::new_master(network, &seed).unwrap();

    let config = Config {
        data_dir: tempdir().unwrap().into_path().to_str().unwrap().to_string(),
        pg_url: env::var("DATABASE_URL").unwrap().to_string(),
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
    let mut lnd = Lnd::with_conf(lnd_exe, &config, &BITCOIND).await.unwrap();
    let lightning = lnd.client.lightning();

    let lnd_address = lightning
        .new_address(NewAddressRequest {
            r#type: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    let lnd_address = Address::from_str(&lnd_address.into_inner().address).unwrap();

    BITCOIND
        .client
        .send_to_address(
            &lnd_address.assume_checked(),
            Amount::from_sat(100_000_000),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

    generate_blocks_and_wait(1).await;

    // wait for lnd to sync
    for _ in 0..240 {
        let balance = lightning
            .wallet_balance(WalletBalanceRequest {})
            .await
            .unwrap();
        let balance = balance.into_inner();
        if balance.confirmed_balance >= 100_000_000 {
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
    let address = rld.wallet.get_new_address().unwrap();

    BITCOIND
        .client
        .send_to_address(
            &address,
            Amount::from_sat(100_000_000),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

    generate_blocks_and_wait(6).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let balance = rld.wallet.balance();
    assert_eq!(balance.confirmed, 100_000_000);
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

pub async fn open_channel_from_lnd(node: &Node, lnd: &mut Lnd) {
    let connection_info = get_lnd_connection_info(lnd).await;
    node.connect_to_peer(connection_info.pubkey, connection_info.socket_addr)
        .await
        .unwrap();

    // wait for stable connection
    tokio::time::sleep(Duration::from_millis(250)).await;

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
