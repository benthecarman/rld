use crate::fees::RldFeeEstimator;
use crate::keys::coin_type_from_network;
use crate::logger::RldLogger;
use anyhow::anyhow;
use bdk_bitcoind_rpc::Emitter;
use bdk_file_store::Store;
use bdk_wallet::chain::{BlockId, ChainOracle, ChainPosition, ConfirmationBlockTime, Indexer};
use bdk_wallet::psbt::PsbtUtils;
use bdk_wallet::template::DescriptorTemplateOut;
use bdk_wallet::{AddressInfo, Balance, ChangeSet};
use bdk_wallet::{KeychainKind, LocalOutput, PersistedWallet, SignOptions, Wallet};
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv};
use bitcoin::psbt::Psbt;
use bitcoin::{Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use bitcoincore_rpc::RpcApi;
use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info, log_trace};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ops::RangeFull;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// A wallet transaction
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetails {
    /// Optional transaction
    pub transaction: Arc<Transaction>,
    /// Transaction id
    pub txid: Txid,
    /// Received value (sats)
    /// Sum of owned outputs of this transaction.
    pub received: Amount,
    /// Sent value (sats)
    /// Sum of owned inputs of this transaction.
    pub sent: Amount,
    /// Fee value in sats if it was available.
    pub fee: Option<Amount>,
    /// Fee Rate if it was available.
    pub fee_rate: Option<FeeRate>,
    /// If the transaction is confirmed, contains height and Unix timestamp of the block containing the
    /// transaction, unconfirmed transaction contains `None`.
    pub chain_position: ChainPosition<ConfirmationBlockTime>,
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
    pub amount: Amount,
    /// Denotes if the output is controlled by the internal wallet
    pub is_our_address: bool,
}

#[derive(Clone)]
pub struct OnChainWallet {
    pub(crate) wallet: Arc<RwLock<PersistedWallet<Store<ChangeSet>>>>,
    pub(crate) store: Arc<RwLock<Store<ChangeSet>>>,
    pub network: Network,
    pub fees: Arc<RldFeeEstimator>,
    stop: Arc<AtomicBool>,
    pub logger: Arc<RldLogger>,
}

impl OnChainWallet {
    pub fn new(
        xprv: Xpriv,
        network: Network,
        data_dir: &PathBuf,
        fees: Arc<RldFeeEstimator>,
        stop: Arc<AtomicBool>,
        logger: Arc<RldLogger>,
    ) -> anyhow::Result<Self> {
        let (receive_descriptor_template, change_descriptor_template) =
            get_tr_descriptors_for_extended_key(xprv, network)?;

        let magic = format!("RLD-{network}");
        if data_dir.exists() {
            log_info!(logger, "Loading wallet");
            let mut store: Store<ChangeSet> = Store::open(magic.as_bytes(), data_dir)?;
            let wallet = Wallet::load()
                .check_network(network)
                .descriptor(KeychainKind::External, Some(receive_descriptor_template))
                .descriptor(KeychainKind::Internal, Some(change_descriptor_template))
                .extract_keys()
                .load_wallet(&mut store)?
                .expect("failed to load wallet");
            let wallet = Arc::new(RwLock::new(wallet));
            let store = Arc::new(RwLock::new(store));
            log_debug!(logger, "Wallet loaded");

            Ok(Self {
                wallet,
                store,
                network,
                fees,
                stop,
                logger,
            })
        } else {
            log_info!(logger, "Creating wallet");
            let mut store: Store<ChangeSet> = Store::create_new(magic.as_bytes(), data_dir)?;
            let wallet = Wallet::create(receive_descriptor_template, change_descriptor_template)
                .network(network)
                .create_wallet(&mut store)
                .expect("failed to create wallet");
            let wallet = Arc::new(RwLock::new(wallet));
            let store = Arc::new(RwLock::new(store));
            log_debug!(logger, "Wallet created");

            Ok(Self {
                wallet,
                store,
                network,
                fees,
                stop,
                logger,
            })
        }
    }

    pub fn start(&self) {
        let wallet = self.wallet.clone();
        let store = self.store.clone();
        let rpc = self.fees.rpc.clone();
        let stop = self.stop.clone();
        let logger = self.logger.clone();
        let network = self.network;

        let sleep_timer = match network {
            Network::Bitcoin | Network::Testnet | Network::Signet => Duration::from_secs(30),
            Network::Regtest => Duration::from_secs(1),
            _ => unreachable!("invalid network"),
        };

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
                        let mut store = store.write().unwrap();
                        wallet.persist(&mut store).unwrap();
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
                    let mut store = store.write().unwrap();
                    wallet.persist(&mut store).unwrap();
                }

                if let Some(mempool) = emitter.mempool().ok().filter(|m| !m.is_empty()) {
                    let mut wallet = wallet.write().unwrap();
                    let start_apply_mempool = Instant::now();
                    wallet.apply_unconfirmed_txs(
                        mempool.iter().map(|(tx, time)| (tx.clone(), *time)),
                    );
                    // wallet.commit().unwrap();
                    log_info!(
                        logger,
                        "Applied unconfirmed transactions in {}s",
                        start_apply_mempool.elapsed().as_secs_f32()
                    );
                }

                if blocks_received > 0 {
                    log_info!(logger, "Blocks received: {blocks_received}");
                }

                sleep(sleep_timer).await;
            }
        });
    }

    pub fn broadcast_transaction(&self, tx: Transaction) -> anyhow::Result<Txid> {
        let txid = tx.compute_txid();
        log_info!(self.logger, "Broadcasting transaction: {txid}");

        if let Err(e) = self.fees.rpc.send_raw_transaction(&tx) {
            log_error!(self.logger, "Failed to broadcast transaction ({txid}): {e}");
            return Err(anyhow!("Failed to broadcast transaction ({txid}): {e}"));
        }

        Ok(txid)
    }

    pub fn balance(&self) -> Balance {
        let wallet = self.wallet.read().unwrap();
        wallet.balance()
    }

    pub fn sync_height(&self) -> anyhow::Result<BlockId> {
        let wallet = self.wallet.read().unwrap();
        Ok(wallet.local_chain().get_chain_tip()?)
    }

    pub fn get_new_address(&self) -> anyhow::Result<AddressInfo> {
        let mut wallet = self.wallet.write().unwrap();
        let address = wallet.next_unused_address(KeychainKind::External);

        Ok(address)
    }

    pub fn get_change_address(&self) -> anyhow::Result<AddressInfo> {
        let mut wallet = self.wallet.write().unwrap();
        let address = wallet.next_unused_address(KeychainKind::Internal);

        Ok(address)
    }

    pub fn sign_psbt(&self, mut psbt: Psbt) -> anyhow::Result<Psbt> {
        let wallet = self.wallet.read().unwrap();

        // need to trust witness_utxo for signing since that's LDK sets in the psbt
        let sign_options = SignOptions {
            trust_witness_utxo: true,
            ..Default::default()
        };
        wallet.sign(&mut psbt, sign_options)?;

        Ok(psbt)
    }

    pub fn create_signed_psbt_to_spk(
        &self,
        spk: ScriptBuf,
        amount: Amount,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Psbt> {
        let mut wallet = self.wallet.try_write().unwrap();

        let fee_rate = fee_rate.unwrap_or_else(|| {
            let sat_per_kwu = self.fees.get_normal_fee_rate();
            FeeRate::from_sat_per_kwu(sat_per_kwu as u64)
        });
        let mut psbt = {
            let mut builder = wallet.build_tx();
            builder.add_recipient(spk, amount).fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        wallet.sign(&mut psbt, SignOptions::default())?;
        Ok(psbt)
    }

    pub fn send_to_address(
        &self,
        addr: Address,
        amount: Amount,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Txid> {
        let psbt = self.create_signed_psbt_to_spk(addr.script_pubkey(), amount, fee_rate)?;

        let raw_transaction = psbt.extract_tx()?;
        let txid = raw_transaction.compute_txid();

        self.broadcast_transaction(raw_transaction)?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    pub fn create_sweep_psbt(
        &self,
        spk: ScriptBuf,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Psbt> {
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
                .fee_rate(fee_rate);
            builder.finish()?
        };
        log_debug!(self.logger, "Unsigned PSBT: {psbt}");
        let _ = wallet.sign(&mut psbt, SignOptions::default())?;
        Ok(psbt)
    }

    pub fn sweep(&self, addr: Address, fee_rate: Option<FeeRate>) -> anyhow::Result<Txid> {
        let psbt = self.create_sweep_psbt(addr.script_pubkey(), fee_rate)?;

        let raw_transaction = psbt.extract_tx()?;
        let txid = raw_transaction.compute_txid();

        self.broadcast_transaction(raw_transaction)?;
        log_debug!(self.logger, "Transaction broadcast! TXID: {txid}");
        Ok(txid)
    }

    pub fn create_unsigned_psbt_to_outputs(
        &self,
        outputs: Vec<TxOut>,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Psbt> {
        if outputs.is_empty() {
            return Err(anyhow!("No outputs provided"));
        }

        let mut wallet = self.wallet.try_write().unwrap();

        let fee_rate = fee_rate.unwrap_or_else(|| {
            let sat_per_kwu = self.fees.get_normal_fee_rate();
            FeeRate::from_sat_per_kwu(sat_per_kwu as u64)
        });
        let mut builder = wallet.build_tx();
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
    ) -> anyhow::Result<Amount> {
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

        let raw_transaction = psbt.extract_tx()?;
        let txid = raw_transaction.compute_txid();

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
                    if wallet.spk_index().is_tx_relevant(&tx.tx_node.tx) {
                        let (sent, received) = wallet
                            .spk_index()
                            .sent_and_received(&tx.tx_node.tx, RangeFull);

                        let fee = wallet.calculate_fee(&tx.tx_node.tx).ok();
                        let fee_rate = wallet.calculate_fee_rate(&tx.tx_node.tx).ok();

                        let output_details = tx
                            .tx_node
                            .tx
                            .output
                            .iter()
                            .enumerate()
                            .map(|(output_index, output)| {
                                let script = output.script_pubkey.as_script();
                                let address = Address::from_script(script, self.network)
                                    .map(|a| a.to_string())
                                    .ok();
                                OutputDetails {
                                    address,
                                    spk: output.script_pubkey.clone(),
                                    output_index,
                                    amount: output.value,
                                    is_our_address: wallet.is_mine(script.into()),
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
                            chain_position: tx.chain_position,
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

                let (sent, received) = wallet.sent_and_received(&tx.tx_node.tx);
                let fee = wallet.calculate_fee(&tx.tx_node.tx).ok();
                let fee_rate = wallet.calculate_fee_rate(&tx.tx_node.tx).ok();
                let output_details = tx
                    .tx_node
                    .tx
                    .output
                    .iter()
                    .enumerate()
                    .map(|(output_index, output)| {
                        let script = output.script_pubkey.as_script();
                        let address = Address::from_script(script, self.network)
                            .map(|a| a.to_string())
                            .ok();
                        OutputDetails {
                            address,
                            spk: output.script_pubkey.clone(),
                            output_index,
                            amount: output.value,
                            is_our_address: wallet.is_mine(script.into()),
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
                    chain_position: tx.chain_position,
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
        let addr = wallet.next_unused_address(KeychainKind::Internal);
        Ok(addr.script_pubkey())
    }

    fn sign_psbt(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
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

        psbt.extract_tx()
            .map_err(|e| log_error!(self.logger, "Could not extract tx: {e:?}"))
    }
}

fn get_tr_descriptors_for_extended_key(
    xprv: Xpriv,
    network: Network,
) -> anyhow::Result<(DescriptorTemplateOut, DescriptorTemplateOut)> {
    let coin_type = coin_type_from_network(network);

    let base_path = DerivationPath::from_str("m/86'")?;
    let derivation_path = base_path.extend([
        ChildNumber::from_hardened_idx(coin_type)?,
        ChildNumber::from_hardened_idx(0)?, // account number
    ]);

    let receive_descriptor_template = bdk_wallet::descriptor!(tr((
        xprv,
        derivation_path.extend([ChildNumber::Normal { index: 0 }])
    )))?;
    let change_descriptor_template = bdk_wallet::descriptor!(tr((
        xprv,
        derivation_path.extend([ChildNumber::Normal { index: 1 }])
    )))?;

    Ok((receive_descriptor_template, change_descriptor_template))
}
