use crate::logger::RldLogger;
use bitcoin::consensus::encode;
use bitcoin::{Transaction, Txid};
use bitcoincore_rpc::RpcApi;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::log_error;
use lightning::util::logger::Logger;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TxBroadcaster {
    /// The RPC client to the Bitcoin Core node
    pub rpc: Arc<bitcoincore_rpc::Client>,
    /// Logger
    pub logger: Arc<RldLogger>,
}

impl TxBroadcaster {
    /// Create a new RLD fee estimator
    pub fn new(rpc: Arc<bitcoincore_rpc::Client>, logger: Arc<RldLogger>) -> Self {
        Self { rpc, logger }
    }
}

impl BroadcasterInterface for TxBroadcaster {
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        // TODO: Rather than calling `sendrawtransaction` in a a loop, we should probably use
        // `submitpackage` once it becomes available.
        for tx in txs {
            let tx_serialized = encode::serialize_hex(tx);
            let tx_json = serde_json::json!(tx_serialized);
            let bitcoind = self.rpc.clone();
            let logger = Arc::clone(&self.logger);
            tokio::spawn(async move {
                // This may error due to RL calling `broadcast_transactions` with the same transaction
                // multiple times, but the error is safe to ignore.
                match bitcoind.call::<Txid>("sendrawtransaction", &[tx_json]) {
                    Ok(_) => {}
                    Err(e) => {
                        let err_str = e.to_string();
                        log_error!(logger,
									   "Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\nTransaction: {}",
									   err_str,
									   tx_serialized);
                        print!("Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\n> ", err_str);
                    }
                }
            });
        }
    }
}
