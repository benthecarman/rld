use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models::channel_closure::ChannelClosure;
use crate::models::channel_open_param::ChannelOpenParam;
use crate::models::invoice::Invoice;
use crate::models::payment::Payment;
use crate::node::{BumpTxEventHandler, ChannelManager, Node, PeerManager};
use crate::onchain::OnChainWallet;
use anyhow::anyhow;
use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::Secp256k1;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use lightning::events::Event;
use lightning::ln::PaymentPreimage;
use lightning::sign::{EntropySource, SpendableOutputDescriptor};
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info};
use std::net::ToSocketAddrs;
use std::sync::Arc;

#[derive(Clone)]
pub struct EventHandler {
    pub channel_manager: Arc<ChannelManager>,
    pub peer_manager: Arc<PeerManager>,
    pub fee_estimator: Arc<RldFeeEstimator>,
    pub wallet: Arc<OnChainWallet>,
    pub keys_manager: Arc<KeysManager>,
    pub bump_tx_event_handler: Arc<BumpTxEventHandler>,
    pub db_pool: Pool<ConnectionManager<PgConnection>>,
    pub logger: Arc<RldLogger>,
}

impl EventHandler {
    pub async fn handle_event(&self, event: Event) {
        if let Err(e) = self.handle_event_internal(event).await {
            log_error!(self.logger, "Error handling event: {e:?}");
        }
    }

    async fn handle_event_internal(&self, event: Event) -> anyhow::Result<()> {
        match event {
            Event::FundingGenerationReady {
                temporary_channel_id,
                counterparty_node_id,
                channel_value_satoshis,
                output_script,
                user_channel_id,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: FundingGenerationReady processing for channel {user_channel_id}"
                );

                let mut conn = self.db_pool.get()?;

                // Get the open parameters for this channel
                let mut params =
                    match ChannelOpenParam::find_by_id(&mut conn, user_channel_id as i32)? {
                        Some(params) => params,
                        None => {
                            anyhow::bail!(
                            "Could not find channel open parameters for channel {user_channel_id}"
                        );
                        }
                    };

                let psbt = match self.wallet.create_signed_psbt_to_spk(
                    output_script,
                    channel_value_satoshis,
                    params.sats_per_vbyte(),
                ) {
                    Ok(psbt) => psbt,
                    Err(e) => {
                        log_error!(
                            self.logger,
                            "ERROR: Could not create signed PSBT for channel {user_channel_id}: {e:?}"
                        );
                        let _ = self.channel_manager.force_close_without_broadcasting_txn(
                            &temporary_channel_id,
                            &counterparty_node_id,
                        );
                        return Err(anyhow!(format!(
                            "Could not create signed PSBT for channel {user_channel_id}: {e:?}"
                        )));
                    }
                };

                let tx = psbt.extract_tx();
                params.set_opening_tx(&tx);

                if let Err(e) = self.channel_manager.funding_transaction_generated(
                    &temporary_channel_id,
                    &counterparty_node_id,
                    tx,
                ) {
                    log_error!(
                        self.logger,
                        "ERROR: Could not send funding transaction to channel manager: {e:?}"
                    );
                    return Err(anyhow!(format!(
                        "Could not send funding transaction to channel manager: {e:?}"
                    )));
                }

                // Save the opening transaction to the database
                params.save(&mut conn)?;

                log_info!(self.logger, "EVENT: FundingGenerationReady success");
                Ok(())
            }
            Event::PaymentClaimable {
                receiver_node_id,
                payment_hash,
                purpose,
                amount_msat,
                counterparty_skimmed_fee_msat: _,
                onion_fields: _,
                via_channel_id: _,
                via_user_channel_id: _,
                claim_deadline: _,
            } => {
                log_debug!(self.logger, "EVENT: PaymentReceived received payment from payment hash {payment_hash} of {amount_msat} msats to {receiver_node_id:?}");

                if let Some(payment_preimage) = purpose.preimage() {
                    self.channel_manager.claim_funds(payment_preimage);
                } else {
                    // if channel_manager doesn't have the preimage, try to find it in the database
                    let mut conn = self.db_pool.get()?;
                    match Invoice::find_by_payment_hash(&mut conn, &payment_hash.0)?
                        .and_then(|x| x.preimage())
                    {
                        None => log_error!(self.logger, "ERROR: No payment preimage found"),
                        Some(preimage) => {
                            self.channel_manager.claim_funds(PaymentPreimage(preimage))
                        }
                    }
                };

                Ok(())
            }
            Event::PaymentClaimed {
                receiver_node_id: _,
                payment_hash,
                amount_msat,
                purpose,
                htlcs: _,
                sender_intended_total_msat: _,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: PaymentClaimed payment hash {payment_hash} of {amount_msat} msats"
                );

                let mut conn = self.db_pool.get()?;
                Invoice::mark_as_paid(
                    &mut conn,
                    payment_hash.0,
                    purpose.preimage().map(|p| p.0),
                    amount_msat as i32,
                )?;

                Ok(())
            }
            Event::ConnectionNeeded { node_id, addresses } => {
                for addr in addresses
                    .into_iter()
                    .flat_map(|x| x.to_socket_addrs().unwrap_or_default())
                {
                    log_debug!(
                        self.logger,
                        "EVENT: ConnectionNeeded connecting to {node_id:?} at {addr}"
                    );
                    match Node::do_connect_peer(self.peer_manager.clone(), node_id, addr).await {
                        Ok(_) => break,
                        Err(e) => log_error!(self.logger, "ERROR: ConnectionNeeded could not connect to {node_id:?} at {addr}: {e}"),
                    }
                }

                Ok(())
            }
            Event::InvoiceRequestFailed { payment_id } => {
                log_info!(
                    self.logger,
                    "EVENT: InvoiceRequestFailed payment_id: {payment_id}"
                );
                Ok(())
            }
            Event::PaymentSent {
                payment_id,
                payment_preimage,
                payment_hash,
                fee_paid_msat,
            } => {
                log_info!(self.logger, "EVENT: PaymentSent payment_id: {payment_id:?}, payment_hash: {payment_hash:?}, fee_paid_msat: {fee_paid_msat:?}");

                let mut conn = self.db_pool.get()?;
                Payment::payment_complete(
                    &mut conn,
                    payment_hash,
                    payment_preimage.0,
                    fee_paid_msat.unwrap_or_default() as i32,
                )?;

                Ok(())
            }
            Event::PaymentFailed {
                payment_id,
                payment_hash,
                reason,
            } => {
                log_info!(self.logger, "EVENT: PaymentFailed payment_id: {payment_id}, payment_hash: {payment_hash:?}, reason: {reason:?}");

                let mut conn = self.db_pool.get()?;
                Payment::payment_failed(&mut conn, payment_hash)?;

                Ok(())
            }
            Event::PaymentPathSuccessful {
                payment_id: _,
                payment_hash,
                path,
            } => {
                let payment_hash = payment_hash.expect("safe after ldk 0.0.104");

                let mut conn = self.db_pool.get()?;
                Payment::add_path(&mut conn, payment_hash, path)?;

                Ok(())
            }
            Event::PaymentPathFailed { .. } => Ok(()),
            Event::ProbeSuccessful { .. } => Ok(()),
            Event::ProbeFailed { .. } => Ok(()),
            Event::PendingHTLCsForwardable { time_forwardable } => {
                log_debug!(
                    self.logger,
                    "EVENT: PendingHTLCsForwardable: {time_forwardable:?}, processing..."
                );

                tokio::time::sleep(time_forwardable).await;
                self.channel_manager.process_pending_htlc_forwards();
                Ok(())
            }
            Event::HTLCIntercepted { .. } => Ok(()),
            Event::SpendableOutputs {
                outputs,
                channel_id,
            } => {
                // Filter out static outputs, we don't want to spend them
                // because they have gone to our BDK wallet.
                // This would only be a waste in fees.
                let output_descriptors = outputs
                    .iter()
                    .filter(|d| match d {
                        SpendableOutputDescriptor::StaticOutput { .. } => false,
                        SpendableOutputDescriptor::DelayedPaymentOutput(_) => true,
                        SpendableOutputDescriptor::StaticPaymentOutput(_) => true,
                    })
                    .collect::<Vec<_>>();

                // If there are no spendable outputs, we don't need to do anything
                if output_descriptors.is_empty() {
                    return Ok(());
                }

                log_debug!(
                    self.logger,
                    "EVENT: SpendableOutputs: {output_descriptors:?} for channel {channel_id:?}, processing..."
                );

                let tx_feerate = self.fee_estimator.get_normal_fee_rate();

                // We set nLockTime to the current height to discourage fee sniping.
                // Occasionally randomly pick a nLockTime even further back, so
                // that transactions that are delayed after signing for whatever reason,
                // e.g. high-latency mix networks and some CoinJoin implementations, have
                // better privacy.
                // Logic copied from core: https://github.com/bitcoin/bitcoin/blob/1d4846a8443be901b8a5deb0e357481af22838d0/src/wallet/spend.cpp#L936
                let mut height = self.channel_manager.current_best_block().height();

                let rand = self.keys_manager.get_secure_random_bytes();
                // 10% of the time
                if (u32::from_be_bytes([rand[0], rand[1], rand[2], rand[3]]) % 10) == 0 {
                    // subtract random number between 0 and 100
                    height -= u32::from_be_bytes([rand[4], rand[5], rand[6], rand[7]]) % 100;
                }

                let locktime = LockTime::from_height(height).ok();

                let spending_tx = self
                    .keys_manager
                    .spend_spendable_outputs(
                        &output_descriptors,
                        Vec::new(),
                        tx_feerate,
                        locktime,
                        &Secp256k1::new(),
                    )
                    .map_err(|_| anyhow!("Failed to spend spendable outputs"))?;

                self.wallet.broadcast_transaction(spending_tx)?;

                Ok(())
            }
            Event::PaymentForwarded { .. } => Ok(()),
            Event::ChannelPending {
                channel_id,
                user_channel_id,
                former_temporary_channel_id: _,
                counterparty_node_id,
                funding_txo: _,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: ChannelPending channel_id: {}, user_channel_id: {}, counterparty_node_id: {}",
                    channel_id,
                    user_channel_id,
                    counterparty_node_id);

                let mut conn = self.db_pool.get()?;
                ChannelOpenParam::mark_success(&mut conn, user_channel_id as i32)?;

                Ok(())
            }
            Event::ChannelReady {
                channel_id,
                user_channel_id,
                counterparty_node_id,
                channel_type,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: ChannelReady channel_id: {}, user_channel_id: {}, counterparty_node_id: {}, channel_type: {}",
                    channel_id,
                    user_channel_id,
                    counterparty_node_id,
                    channel_type);

                Ok(())
            }
            Event::ChannelClosed {
                channel_id,
                reason,
                user_channel_id,
                counterparty_node_id: node_id,
                channel_capacity_sats,
                channel_funding_txo,
            } => {
                // if we still have channel open params, then it was just a failed channel open
                // we should not persist this as a closed channel and just delete the channel open params
                let mut conn = self.db_pool.get()?;
                let id = user_channel_id as i32;
                if let Ok(Some(_)) = ChannelOpenParam::find_by_id(&mut conn, id) {
                    // should we delete from db?
                    return Ok(());
                };

                let node_id = node_id.expect("Safe after ldk 117");
                log_debug!(
                    self.logger,
                    "EVENT: Channel {channel_id}  to {node_id} of size {} closed due to: {reason}",
                    channel_capacity_sats
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                );

                ChannelClosure::create(
                    &mut conn,
                    id,
                    node_id,
                    channel_funding_txo.map(|x| x.into_bitcoin_outpoint()),
                    reason.to_string(),
                )?;

                Ok(())
            }
            Event::DiscardFunding { .. } => {
                // A "real" node should probably "lock" the UTXOs spent in funding transactions until
                // the funding transaction either confirms, or this event is generated.
                log_debug!(self.logger, "EVENT: DiscardFunding, ignored");
                Ok(())
            }
            Event::OpenChannelRequest {
                temporary_channel_id,
                counterparty_node_id,
                funding_satoshis: _,
                push_msat: _,
                channel_type: _,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: OpenChannelRequest incoming: {counterparty_node_id}"
                );

                // todo calculate deterministically
                let user_channel_id = 0;

                let result = self.channel_manager.accept_inbound_channel(
                    &temporary_channel_id,
                    &counterparty_node_id,
                    user_channel_id,
                );

                match result {
                    Ok(_) => log_debug!(self.logger, "EVENT: OpenChannelRequest accepted"),
                    Err(e) => log_debug!(self.logger, "EVENT: OpenChannelRequest error: {e:?}"),
                };

                Ok(())
            }
            Event::HTLCHandlingFailed { .. } => Ok(()),
            Event::BumpTransaction(event) => {
                log_debug!(self.logger, "EVENT: BumpTransaction: {event:?}");
                self.bump_tx_event_handler.handle_event(&event);
                Ok(())
            },
        }
    }
}
