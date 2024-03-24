use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models::invoice::Invoice;
use crate::ChannelManager;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use lightning::events::Event;
use lightning::ln::PaymentPreimage;
use lightning::util::logger::Logger;
use lightning::{log_debug, log_error};
use std::sync::Arc;

#[derive(Clone)]
pub struct EventHandler {
    pub channel_manager: Arc<ChannelManager>,
    pub fee_estimator: Arc<RldFeeEstimator>,
    // wallet: Arc<OnChainWallet<S>>,
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
            Event::FundingGenerationReady { .. } => Ok(()),
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
            Event::ChannelPending { .. } => Ok(()),
            Event::ChannelReady { .. } => Ok(()),
            Event::ChannelClosed { .. } => Ok(()),
            Event::DiscardFunding { .. } => Ok(()),
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
