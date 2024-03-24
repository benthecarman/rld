use crate::fees::RldFeeEstimator;
use crate::keys::coin_type_from_network;
use crate::logger::RldLogger;
use bdk::template::DescriptorTemplateOut;
use bdk::Wallet;
use bdk_bitcoind_rpc::Emitter;
use bdk_file_store::Store;
use bitcoin::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::{Block, Network, Transaction};
use lightning::log_info;
use lightning::util::logger::Logger;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Clone)]
pub(crate) struct OnChainWallet {
    pub(crate) wallet: Arc<RwLock<Wallet<Store<bdk::wallet::ChangeSet>>>>,
    pub network: Network,
    pub fees: Arc<RldFeeEstimator>,
    pub logger: Arc<RldLogger>,
}

impl OnChainWallet {
    pub fn new(
        xprv: ExtendedPrivKey,
        network: Network,
        data_dir: &PathBuf,
        fees: Arc<RldFeeEstimator>,
        logger: Arc<RldLogger>,
    ) -> anyhow::Result<Self> {
        let (receive_descriptor_template, change_descriptor_template) =
            get_tr_descriptors_for_extended_key(xprv, network)?;

        let magic = format!("RLD-{network}");
        let store = Store::open_or_create_new(magic.as_bytes(), data_dir)?;
        let wallet = Wallet::new_or_load(
            receive_descriptor_template,
            Some(change_descriptor_template),
            store,
            network,
        )?;
        let wallet = Arc::new(RwLock::new(wallet));
        Ok(Self {
            wallet,
            network,
            fees,
            logger,
        })
    }

    pub fn start(&self) {
        let wallet = self.wallet.clone();
        let rpc = self.fees.rpc.clone();
        let logger = self.logger.clone();

        tokio::spawn(async move {
            loop {
                let wallet_tip = {
                    let w = wallet.read().unwrap();
                    w.latest_checkpoint()
                };
                let (sender, receiver) = sync_channel::<Emission>(21);

                let emitter_tip = wallet_tip.clone();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let height = emitter_tip.height();
                    let mut emitter = Emitter::new(rpc.as_ref(), emitter_tip, height);
                    while let Some(emission) = emitter.next_block().unwrap() {
                        sender.send(Emission::Block(emission)).unwrap();
                    }
                    sender
                        .send(Emission::Mempool(emitter.mempool().unwrap()))
                        .unwrap();
                });

                let mut blocks_received = 0_usize;
                for emission in receiver {
                    match emission {
                        Emission::Block(block_emission) => {
                            blocks_received += 1;
                            let height = block_emission.block_height();
                            let hash = block_emission.block_hash();
                            let connected_to = block_emission.connected_to();
                            let start_apply_block = Instant::now();
                            let mut wallet = wallet.write().unwrap();
                            wallet
                                .apply_block_connected_to(
                                    &block_emission.block,
                                    height,
                                    connected_to,
                                )
                                .unwrap();
                            wallet.commit().unwrap();
                            let elapsed = start_apply_block.elapsed().as_secs_f32();
                            println!(
                                "Applied block {} at height {} in {}s",
                                hash, height, elapsed
                            );
                        }
                        Emission::Mempool(mempool_emission) => {
                            let mut wallet = wallet.write().unwrap();
                            let start_apply_mempool = Instant::now();
                            wallet.apply_unconfirmed_txs(
                                mempool_emission.iter().map(|(tx, time)| (tx, *time)),
                            );
                            wallet.commit().unwrap();
                            println!(
                                "Applied unconfirmed transactions in {}s",
                                start_apply_mempool.elapsed().as_secs_f32()
                            );
                            break;
                        }
                    }
                }

                if blocks_received > 0 {
                    log_info!(logger, "Blocks received: {}", blocks_received);
                }

                sleep(Duration::from_secs(30)).await;
            }
        });
    }
}

#[derive(Debug)]
enum Emission {
    Block(bdk_bitcoind_rpc::BlockEvent<Block>),
    Mempool(Vec<(Transaction, u64)>),
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
