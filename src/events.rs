use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models::channel_closure::ChannelClosure;
use crate::models::channel_open_param::ChannelOpenParam;
use crate::models::invoice::Invoice;
use crate::node::ChannelManager;
use crate::onchain::OnChainWallet;
use anyhow::anyhow;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use lightning::events::Event;
use lightning::ln::PaymentPreimage;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error, log_info};
use std::sync::Arc;

#[derive(Clone)]
pub struct EventHandler {
    pub channel_manager: Arc<ChannelManager>,
    pub fee_estimator: Arc<RldFeeEstimator>,
    pub wallet: Arc<OnChainWallet>,
    pub keys_manager: Arc<KeysManager>,
    // persister: Arc<MutinyNodePersister<S>>,
    // bump_tx_event_handler: Arc<BumpTxEventHandler<S>>,
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
            Event::PaymentClaimed { .. } => Ok(()),
            Event::ConnectionNeeded { .. } => Ok(()),
            Event::InvoiceRequestFailed { .. } => Ok(()),
            Event::PaymentSent { .. } => Ok(()),
            Event::PaymentFailed { .. } => Ok(()),
            Event::PaymentPathSuccessful { .. } => Ok(()),
            Event::PaymentPathFailed { .. } => Ok(()),
            Event::ProbeSuccessful { .. } => Ok(()),
            Event::ProbeFailed { .. } => Ok(()),
            Event::PendingHTLCsForwardable { .. } => Ok(()),
            Event::HTLCIntercepted { .. } => Ok(()),
            Event::SpendableOutputs { .. } => Ok(()),
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
            Event::BumpTransaction(_) => Ok(()),
        }
    }
}
