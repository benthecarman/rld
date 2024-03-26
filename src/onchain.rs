use crate::fees::RldFeeEstimator;
use crate::keys::coin_type_from_network;
use crate::logger::RldLogger;
use anyhow::anyhow;
use bdk::chain::{BlockId, ConfirmationTime};
use bdk::template::DescriptorTemplateOut;
use bdk::wallet::{AddressIndex, AddressInfo, Balance};
use bdk::{FeeRate, SignOptions, Wallet};
use bdk_bitcoind_rpc::Emitter;
use bdk_file_store::Store;
use bitcoin::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::psbt::PartiallySignedTransaction;
use bitcoin::{Network, ScriptBuf, Transaction};
use bitcoincore_rpc::RpcApi;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info, log_trace, log_warn};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::time::sleep;

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
            FeeRate::from_sat_per_kwu(sat_per_kwu as f32)
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
