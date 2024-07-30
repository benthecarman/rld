use crate::logger::RldLogger;
use crate::onchain::OnChainWallet;
use bdk_wallet::chain::ConfirmationTime;
use bitcoin::consensus::encode;
use bitcoin::{Transaction, Txid};
use bitcoincore_rpc::RpcApi;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::log_error;
use lightning::util::logger::Logger;
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Clone)]
pub struct TxBroadcaster {
    /// The RPC client to the Bitcoin Core node
    pub rpc: Arc<bitcoincore_rpc::Client>,
    pub wallet: Arc<OnChainWallet>,
    /// Logger
    pub logger: Arc<RldLogger>,
}

impl TxBroadcaster {
    /// Create a new RLD fee estimator
    pub fn new(
        rpc: Arc<bitcoincore_rpc::Client>,
        wallet: Arc<OnChainWallet>,
        logger: Arc<RldLogger>,
    ) -> Self {
        Self {
            rpc,
            wallet,
            logger,
        }
    }
}

impl BroadcasterInterface for TxBroadcaster {
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        // TODO: Rather than calling `sendrawtransaction` in a a loop, we should probably use
        // `submitpackage` once it becomes available.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let pos = ConfirmationTime::Unconfirmed { last_seen: now };
        for tx in txs {
            let tx_serialized = encode::serialize_hex(tx);
            let tx_json = serde_json::json!(tx_serialized);
            let bitcoind = self.rpc.clone();
            let logger = Arc::clone(&self.logger);
            // This may error due to RL calling `broadcast_transactions` with the same transaction
            // multiple times, but the error is safe to ignore.
            match bitcoind.call::<Txid>("sendrawtransaction", &[tx_json]) {
                Ok(txid) => {
                    let tx = tx.to_owned().clone();
                    if let Err(e) = self.wallet.insert_tx(tx, pos, None) {
                        log_error!(
                            logger,
                            "Failed to insert transaction {txid} into wallet: {e}"
                        );
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    log_error!(logger, "Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {err_str}\nTransaction: {tx_serialized}");
                }
            }
        }
    }
}
