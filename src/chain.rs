use crate::logger::RldLogger;
use crate::onchain::OnChainWallet;
use bitcoin::consensus::encode;
use bitcoin::Transaction;
use bitcoincore_rpc::RpcApi;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::log_error;
use lightning::util::logger::Logger;
use std::sync::Arc;

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
        // encode txs to hex
        let txn = txs
            .iter()
            .map(|tx| encode::serialize_hex(tx))
            .collect::<Vec<_>>();

        let tx_json: serde_json::Value;
        let res = if txn.len() == 1 {
            tx_json = serde_json::json!(txn[0]);
            self.rpc
                .call::<serde_json::Value>("sendrawtransaction", &[tx_json.clone()])
        } else {
            tx_json = serde_json::json!(txn);
            self.rpc
                .call::<serde_json::Value>("submitpackage", &[tx_json.clone()])
        };
        match res {
            Ok(_) => {
                for tx in txs {
                    let tx = tx.to_owned().clone();
                    let txid = tx.compute_txid();
                    if let Err(e) = self.wallet.insert_tx(tx) {
                        log_error!(
                            self.logger,
                            "Failed to insert transaction {txid} into wallet: {e}"
                        );
                    }
                }
            }
            Err(e) => {
                log_error!(self.logger, "Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {e}\nTransactions: {tx_json}");
            }
        }
    }
}
