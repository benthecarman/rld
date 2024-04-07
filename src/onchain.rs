use crate::fees::RldFeeEstimator;
use crate::keys::coin_type_from_network;
use crate::logger::RldLogger;
use anyhow::anyhow;
use bdk::chain::indexed_tx_graph::Indexer;
use bdk::chain::{BlockId, ConfirmationTime};
use bdk::psbt::PsbtUtils;
use bdk::template::DescriptorTemplateOut;
use bdk::wallet::{AddressIndex, AddressInfo, Balance};
use bdk::{LocalOutput, SignOptions, Wallet};
use bdk_bitcoind_rpc::Emitter;
use bdk_file_store::Store;
use bitcoin::address::Payload;
use bitcoin::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::{Address, FeeRate, Network, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use bitcoincore_rpc::RpcApi;
use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info, log_trace, log_warn};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::time::sleep;

/// A wallet transaction
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetails {
    /// Optional transaction
    pub transaction: Transaction,
    /// Transaction id
    pub txid: Txid,
    /// Received value (sats)
    /// Sum of owned outputs of this transaction.
    pub received: u64,
    /// Sent value (sats)
    /// Sum of owned inputs of this transaction.
    pub sent: u64,
    /// Fee value in sats if it was available.
    pub fee: Option<u64>,
    /// Fee Rate if it was available.
    pub fee_rate: Option<FeeRate>,
    /// If the transaction is confirmed, contains height and Unix timestamp of the block containing the
    /// transaction, unconfirmed transaction contains `None`.
    pub confirmation_time: ConfirmationTime,
    /// Details of the outputs of the transaction
    pub previous_outpoints: Vec<PreviousOutpoint>,
    /// Details of the outputs of the transaction
    pub output_details: Vec<OutputDetails>,
}

/// Details of an input in a transaction
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PreviousOutpoint {
    /// The outpoint
    pub outpoint: OutPoint,
    /// Denotes if the output is controlled by the internal wallet
    pub is_our_output: bool,
}

/// Details of an output in a transaction
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutputDetails {
    /// The address
    pub address: Option<String>,
    /// The script pub key
    pub spk: ScriptBuf,
    /// The output index used in the raw transaction
    pub output_index: usize,
    /// The value of the output coin in satoshis
    pub amount: u64,
    /// Denotes if the output is controlled by the internal wallet
    pub is_our_address: bool,
}

#[derive(Clone)]
pub(crate) struct OnChainWallet {
    pub(crate) wallet: Arc<RwLock<Wallet<Store<bdk::wallet::ChangeSet>>>>,
    pub network: Network,
    pub fees: Arc<RldFeeEstimator>,
    stop: Arc<AtomicBool>,
    pub logger: Arc<RldLogger>,
}

impl OnChainWallet {
    pub fn new(
        xprv: ExtendedPrivKey,
        network: Network,
        data_dir: &PathBuf,
        fees: Arc<RldFeeEstimator>,
        stop: Arc<AtomicBool>,
        logger: Arc<RldLogger>,
    ) -> anyhow::Result<Self> {
        let (receive_descriptor_template, change_descriptor_template) =
            get_tr_descriptors_for_extended_key(xprv, network)?;

        log_info!(logger, "Loading wallet");
        let magic = format!("RLD-{network}");
        let store = Store::open_or_create_new(magic.as_bytes(), data_dir)?;
        let wallet = Wallet::new_or_load(
            receive_descriptor_template,
            Some(change_descriptor_template),
            store,
            network,
        )?;
        let wallet = Arc::new(RwLock::new(wallet));
        log_debug!(logger, "Wallet loaded");

        Ok(Self {
            wallet,
            network,
            fees,
            stop,
            logger,
        })
    }

    pub fn start(&self) {
        let wallet = self.wallet.clone();
        let rpc = self.fees.rpc.clone();
        let stop = self.stop.clone();
        let logger = self.logger.clone();

        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                log_debug!(logger, "Starting wallet sync thread");

                let wallet_tip = {
                    let w = wallet.read().unwrap();
                    w.latest_checkpoint()
                };

                let mut height = wallet_tip.height();

                // if it is a fresh wallet, just sync the most recent 100 blocks
                if wallet_tip.height() == 0 {
                    log_trace!(logger, "Fresh wallet, getting tip to start from");
                    height = rpc.get_block_count().unwrap().saturating_sub(100) as u32;
                }

                log_debug!(logger, "Wallet tip: {}", wallet_tip.height());
                log_debug!(logger, "Starting emitter");

                let mut blocks_received = 0_u64;
                let mut emitter = Emitter::new(rpc.as_ref(), wallet_tip, height);
                while let Ok(Some(block_emission)) = emitter.next_block() {
                    let hash = block_emission.block_hash();
                    let height = block_emission.block_height();
                    let connected_to = block_emission.connected_to();
                    let start_apply_block = Instant::now();
                    let mut wallet = wallet.write().unwrap();
                    wallet
                        .apply_block_connected_to(&block_emission.block, height, connected_to)
                        .unwrap();

                    // Commit every 100 blocks
                    if blocks_received % 100 == 0 {
                        wallet.commit().unwrap();
                        // Check if we should stop
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }

                    let elapsed = start_apply_block.elapsed().as_millis();
                    log_info!(
                        logger,
                        "Applied block {hash} at height {height} in {elapsed}ms",
                    );
                    blocks_received += 1;
                }

                // Commit the remaining blocks
                {
                    let mut wallet = wallet.write().unwrap();
                    wallet.commit().unwrap();
                }

                if let Ok(mempool) = emitter.mempool() {
                    let mut wallet = wallet.write().unwrap();
                    let start_apply_mempool = Instant::now();
                    wallet.apply_unconfirmed_txs(mempool.iter().map(|(tx, time)| (tx, *time)));
                    wallet.commit().unwrap();
                    log_info!(
                        logger,
                        "Applied unconfirmed transactions in {}s",
                        start_apply_mempool.elapsed().as_secs_f32()
                    );
                    break;
                }

                if blocks_received > 0 {
                    log_info!(logger, "Blocks received: {blocks_received}");
                }

                sleep(Duration::from_secs(30)).await;
            }
        });
    }

    pub fn broadcast_transaction(&self, tx: Transaction) -> anyhow::Result<()> {
        let txid = tx.txid();
        log_info!(self.logger, "Broadcasting transaction: {txid}");

        if let Err(e) = self.fees.rpc.send_raw_transaction(&tx) {
            log_error!(self.logger, "Failed to broadcast transaction ({txid}): {e}");
            return Err(anyhow!("Failed to broadcast transaction ({txid}): {e}"));
        } else if let Err(e) = self.insert_tx(
            tx,
            ConfirmationTime::Unconfirmed {
                last_seen: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_secs(),
            },
            None,
        ) {
            log_warn!(self.logger, "ERROR: Could not sync broadcasted tx ({txid}), will be synced in next iteration: {e:?}");
        }

        Ok(())
    }

    pub(crate) fn insert_tx(
        &self,
        tx: Transaction,
        position: ConfirmationTime,
        block_id: Option<BlockId>,
    ) -> anyhow::Result<()> {
        let txid = tx.txid();
        match position {
            ConfirmationTime::Confirmed { .. } => {
                // if the transaction is confirmed and we have the block id,
                // we can insert it directly
                if let Some(block_id) = block_id {
                    let mut wallet = self.wallet.try_write().unwrap();
                    wallet.insert_checkpoint(block_id)?;
                    wallet.insert_tx(tx, position)?;
                } else {
                    return Ok(());
                }
            }
            ConfirmationTime::Unconfirmed { .. } => {
                // if the transaction is unconfirmed, we can just insert it
                let mut wallet = self.wallet.try_write().unwrap();

                // if we already have the transaction, we don't need to insert it
                if wallet.get_tx(txid).is_none() {
                    // insert tx and commit changes
                    wallet.insert_tx(tx, position)?;
                    wallet.commit()?;
                } else {
                    log_debug!(
                        self.logger,
                        "Tried to insert already existing transaction ({txid})",
                    )
                }
            }
        }

        Ok(())
    }

    pub fn balance(&self) -> Balance {
        let wallet = self.wallet.read().unwrap();
        wallet.get_balance()
    }

    pub fn get_new_address(&self) -> anyhow::Result<AddressInfo> {
        let mut wallet = self.wallet.write().unwrap();
        let address = wallet.try_get_address(AddressIndex::New)?;

        Ok(address)
    }

    pub fn create_signed_psbt_to_spk(
        &self,
        spk: ScriptBuf,
        amount: u64,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<PartiallySignedTransaction> {
        let mut wallet = self.wallet.try_write().unwrap();

        let fee_rate = fee_rate.unwrap_or_else(|| {
            let sat_per_kwu = self.fees.get_normal_fee_rate();
            FeeRate::from_sat_per_kwu(sat_per_kwu as u64)
        });
        let mut psbt = {
            let mut builder = wallet.build_tx();
            builder
                .add_recipient(spk, amount)
                .enable_rbf()
                .fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        wallet.sign(&mut psbt, SignOptions::default())?;
        Ok(psbt)
    }

    pub fn send_to_address(
        &self,
        addr: Address,
        amount: u64,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Txid> {
        let psbt = self.create_signed_psbt_to_spk(addr.script_pubkey(), amount, fee_rate)?;

        let raw_transaction = psbt.extract_tx();
        let txid = raw_transaction.txid();

        self.broadcast_transaction(raw_transaction)?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    pub fn create_sweep_psbt(
        &self,
        spk: ScriptBuf,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<PartiallySignedTransaction> {
        let mut wallet = self.wallet.try_write().unwrap();

        let fee_rate = fee_rate.unwrap_or_else(|| {
            let sat_per_kwu = self.fees.get_normal_fee_rate();
            FeeRate::from_sat_per_kwu(sat_per_kwu as u64)
        });

        let mut psbt = {
            let mut builder = wallet.build_tx();
            builder
                .drain_wallet() // Spend all outputs in this wallet.
                .drain_to(spk)
                .enable_rbf()
                .fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        let _ = wallet.sign(&mut psbt, SignOptions::default())?;
        Ok(psbt)
    }

    pub fn sweep(&self, addr: Address, fee_rate: Option<FeeRate>) -> anyhow::Result<Txid> {
        let psbt = self.create_sweep_psbt(addr.script_pubkey(), fee_rate)?;

        let raw_transaction = psbt.extract_tx();
        let txid = raw_transaction.txid();

        self.broadcast_transaction(raw_transaction)?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    pub fn create_unsigned_psbt_to_outputs(
        &self,
        outputs: Vec<TxOut>,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<PartiallySignedTransaction> {
        if outputs.is_empty() {
            return Err(anyhow!("No outputs provided"));
        }

        let mut wallet = self.wallet.try_write().unwrap();

        let fee_rate = fee_rate.unwrap_or_else(|| {
            let sat_per_kwu = self.fees.get_normal_fee_rate();
            FeeRate::from_sat_per_kwu(sat_per_kwu as u64)
        });
        let mut builder = wallet.build_tx();
        builder.enable_rbf();
        builder.fee_rate(fee_rate);

        for output in outputs {
            builder.add_recipient(output.script_pubkey, output.value);
        }

        Ok(builder.finish()?)
    }

    pub fn estimate_fee_to_outputs(
        &self,
        outputs: Vec<TxOut>,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<u64> {
        let psbt = self.create_unsigned_psbt_to_outputs(outputs, fee_rate)?;
        psbt.fee_amount()
            .ok_or_else(|| anyhow!("Could not estimate fee"))
    }

    pub fn send_to_outputs(
        &self,
        outputs: Vec<TxOut>,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Txid> {
        let mut psbt = self.create_unsigned_psbt_to_outputs(outputs, fee_rate)?;
        {
            let wallet = self.wallet.try_read().unwrap();
            wallet.sign(&mut psbt, SignOptions::default())?;
        }

        let raw_transaction = psbt.extract_tx();
        let txid = raw_transaction.txid();

        self.broadcast_transaction(raw_transaction)?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    /// List all UTXOs in the wallet
    pub fn list_utxos(&self) -> anyhow::Result<Vec<LocalOutput>> {
        Ok(self.wallet.try_read().unwrap().list_unspent().collect())
    }

    /// Lists all the on-chain transactions in the wallet.
    /// These are sorted by confirmation time.
    pub fn list_transactions(&self) -> anyhow::Result<Vec<TransactionDetails>> {
        if let Ok(wallet) = self.wallet.try_read() {
            let my_outpoints = wallet
                .list_output()
                .map(|o| o.outpoint)
                .collect::<HashSet<OutPoint>>();

            let txs = wallet
                .transactions()
                .filter_map(|tx| {
                    // skip txs that were not relevant to our bdk wallet
                    if wallet.spk_index().is_tx_relevant(tx.tx_node.tx) {
                        let (sent, received) = wallet.spk_index().sent_and_received(tx.tx_node.tx);

                        let fee = wallet.calculate_fee(tx.tx_node.tx).ok();
                        let fee_rate = wallet.calculate_fee_rate(tx.tx_node.tx).ok();

                        let output_details = tx
                            .tx_node
                            .tx
                            .output
                            .iter()
                            .enumerate()
                            .map(|(output_index, output)| {
                                let script = output.script_pubkey.as_script();
                                let address = Payload::from_script(script)
                                    .ok()
                                    .map(|p| Address::new(self.network, p).to_string());
                                OutputDetails {
                                    address,
                                    spk: output.script_pubkey.clone(),
                                    output_index,
                                    amount: output.value,
                                    is_our_address: wallet.is_mine(script),
                                }
                            })
                            .collect();

                        let previous_outpoints = tx
                            .tx_node
                            .tx
                            .input
                            .iter()
                            .map(|p| PreviousOutpoint {
                                outpoint: p.previous_output,
                                is_our_output: my_outpoints.contains(&p.previous_output),
                            })
                            .collect();

                        Some(TransactionDetails {
                            transaction: tx.tx_node.tx.clone(),
                            txid: tx.tx_node.txid,
                            received,
                            sent,
                            fee,
                            fee_rate,
                            confirmation_time: tx.chain_position.cloned().into(),
                            previous_outpoints,
                            output_details,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            return Ok(txs);
        }
        log_error!(
            self.logger,
            "Could not get wallet lock to list transactions"
        );
        Err(anyhow::anyhow!(
            "Could not get wallet lock to list transactions"
        ))
    }

    /// Gets the details of a specific on-chain transaction.
    pub fn get_transaction(&self, txid: Txid) -> anyhow::Result<Option<TransactionDetails>> {
        let wallet = self.wallet.try_read().unwrap();
        let bdk_tx = wallet.get_tx(txid);

        match bdk_tx {
            None => Ok(None),
            Some(tx) => {
                let my_outpoints = wallet
                    .list_output()
                    .map(|o| o.outpoint)
                    .collect::<HashSet<OutPoint>>();

                let (sent, received) = wallet.sent_and_received(tx.tx_node.tx);
                let fee = wallet.calculate_fee(tx.tx_node.tx).ok();
                let fee_rate = wallet.calculate_fee_rate(tx.tx_node.tx).ok();
                let output_details = tx
                    .tx_node
                    .tx
                    .output
                    .iter()
                    .enumerate()
                    .map(|(output_index, output)| {
                        let script = output.script_pubkey.as_script();
                        let address = Payload::from_script(script)
                            .ok()
                            .map(|p| Address::new(self.network, p).to_string());
                        OutputDetails {
                            address,
                            spk: output.script_pubkey.clone(),
                            output_index,
                            amount: output.value,
                            is_our_address: wallet.is_mine(script),
                        }
                    })
                    .collect();
                let previous_outpoints = tx
                    .tx_node
                    .tx
                    .input
                    .iter()
                    .map(|p| PreviousOutpoint {
                        outpoint: p.previous_output,
                        is_our_output: my_outpoints.contains(&p.previous_output),
                    })
                    .collect();
                let details = TransactionDetails {
                    transaction: tx.tx_node.tx.to_owned(),
                    txid,
                    received,
                    sent,
                    fee,
                    fee_rate,
                    confirmation_time: tx.chain_position.cloned().into(),
                    previous_outpoints,
                    output_details,
                };

                Ok(Some(details))
            }
        }
    }
}

impl WalletSource for OnChainWallet {
    fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
        let wallet = self.wallet.try_read().map_err(|_| ())?;
        let utxos = wallet
            .list_unspent()
            .map(|u| Utxo {
                outpoint: u.outpoint,
                output: u.txout,
                satisfaction_weight: 4 + 2 + 64,
            })
            .collect();

        Ok(utxos)
    }

    fn get_change_script(&self) -> Result<ScriptBuf, ()> {
        let mut wallet = self.wallet.try_write().map_err(|_| ())?;
        let addr = wallet
            .try_get_internal_address(AddressIndex::New)
            .map_err(|_| ())?
            .address;
        Ok(addr.script_pubkey())
    }

    fn sign_psbt(&self, mut psbt: PartiallySignedTransaction) -> Result<Transaction, ()> {
        let wallet = self.wallet.try_read().map_err(|e| {
            log_error!(
                self.logger,
                "Could not get wallet lock to sign transaction: {e:?}"
            )
        })?;

        // need to trust witness_utxo for signing since that's LDK sets in the psbt
        let sign_options = SignOptions {
            trust_witness_utxo: true,
            ..Default::default()
        };
        wallet
            .sign(&mut psbt, sign_options)
            .map_err(|e| log_error!(self.logger, "Could not sign transaction: {e:?}"))?;

        Ok(psbt.extract_tx())
    }
}

fn get_tr_descriptors_for_extended_key(
    xprv: ExtendedPrivKey,
    network: Network,
) -> anyhow::Result<(DescriptorTemplateOut, DescriptorTemplateOut)> {
    let coin_type = coin_type_from_network(network);

    let base_path = DerivationPath::from_str("m/86'")?;
    let derivation_path = base_path.extend([
        ChildNumber::from_hardened_idx(coin_type)?,
        ChildNumber::from_hardened_idx(0)?, // account number
    ]);

    let receive_descriptor_template = bdk::descriptor!(tr((
        xprv,
        derivation_path.extend([ChildNumber::Normal { index: 0 }])
    )))?;
    let change_descriptor_template = bdk::descriptor!(tr((
        xprv,
        derivation_path.extend([ChildNumber::Normal { index: 1 }])
    )))?;

    Ok((receive_descriptor_template, change_descriptor_template))
}
