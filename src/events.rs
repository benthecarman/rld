use crate::channel_acceptor::{ChannelAcceptor, ChannelAcceptorRequest};
use crate::fees::RldFeeEstimator;
use crate::keys::KeysManager;
use crate::logger::RldLogger;
use crate::models::channel::Channel;
use crate::models::channel_closure::ChannelClosure;
use crate::models::payment::Payment;
use crate::models::receive::{InvoiceStatus, Receive};
use crate::models::received_htlc::ReceivedHtlc;
use crate::models::routed_payment::RoutedPayment;
use crate::node::{BumpTxEventHandler, ChannelManager, Node, PeerManager};
use crate::onchain::OnChainWallet;
use anyhow::anyhow;
use bitcoin::absolute::LockTime;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::Secp256k1;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::{Connection, PgConnection};
use lightning::events::{ClosureReason, Event, PaymentPurpose, ReplayEvent};
use lightning::ln::PaymentPreimage;
use lightning::sign::{EntropySource, SpendableOutputDescriptor};
use lightning::util::logger::Logger;
use lightning::util::ser::Writeable;
use lightning::{log_debug, log_error, log_info, log_trace};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::RwLock;

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

    // broadcast channels
    pub invoice_broadcast: Sender<Receive>,

    pub channel_acceptor: Arc<RwLock<Option<ChannelAcceptor>>>,
}

impl EventHandler {
    pub async fn handle_event(&self, event: Event) -> Result<(), ReplayEvent> {
        match self.handle_event_internal(event).await {
            Ok(_) => Ok(()),
            Err(e) => {
                log_error!(self.logger, "Error handling event: {e:?}");
                Err(ReplayEvent())
            }
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
                let mut params = match Channel::find_by_id(&mut conn, user_channel_id as i32)? {
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
                            "Could not create signed PSBT for channel".to_string(),
                        );
                        return Err(anyhow!(format!(
                            "Could not create signed PSBT for channel {user_channel_id}: {e:?}"
                        )));
                    }
                };

                let tx = psbt.extract_tx()?;
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
            Event::FundingTxBroadcastSafe { .. } => {
                // we should never get this event
                log_debug!(self.logger, "EVENT: FundingTxBroadcastSafe");
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

                let mut conn = self.db_pool.get()?;
                let receive = Receive::find_by_payment_hash(&mut conn, &payment_hash.0)?;

                if receive.as_ref().is_some_and(|r| {
                    matches!(r.status(), InvoiceStatus::Expired | InvoiceStatus::Canceled)
                }) {
                    log_info!(self.logger, "EVENT: PaymentReceived received canceled payment from payment hash {payment_hash} of {amount_msat} msats to {receiver_node_id:?}");
                    self.channel_manager.fail_htlc_backwards(&payment_hash);
                    return Ok(());
                }

                if let Some(payment_preimage) = purpose.preimage() {
                    self.channel_manager.claim_funds(payment_preimage);
                } else {
                    // if channel_manager doesn't have the preimage, try to find it in the database
                    match Receive::find_by_payment_hash(&mut conn, &payment_hash.0)?
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
                htlcs,
                sender_intended_total_msat: _,
                onion_fields: _, // todo
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: PaymentClaimed payment hash {payment_hash} of {amount_msat} msats"
                );

                let mut conn = self.db_pool.get()?;

                let inv = conn.transaction::<_, anyhow::Error, _>(|conn| {
                    let inv = match purpose {
                        PaymentPurpose::Bolt11InvoicePayment {
                            payment_preimage, ..
                        } => Receive::mark_as_paid(
                            conn,
                            payment_hash.0,
                            payment_preimage.map(|p| p.0),
                            amount_msat as i64,
                        )?,
                        PaymentPurpose::SpontaneousPayment(payment_preimage) => {
                            Receive::create_keysend(
                                conn,
                                payment_hash.0,
                                payment_preimage.0,
                                amount_msat as i64,
                            )?
                        }
                        PaymentPurpose::Bolt12OfferPayment {
                            payment_preimage, ..
                        } => Receive::mark_as_paid(
                            conn,
                            payment_hash.0,
                            payment_preimage.map(|p| p.0),
                            amount_msat as i64,
                        )?,
                        PaymentPurpose::Bolt12RefundPayment {
                            payment_preimage, ..
                        } => Receive::mark_as_paid(
                            conn,
                            payment_hash.0,
                            payment_preimage.map(|p| p.0),
                            amount_msat as i64,
                        )?,
                    };

                    for htlc in htlcs {
                        ReceivedHtlc::create(
                            conn,
                            inv.id,
                            htlc.value_msat as i64,
                            htlc.user_channel_id as i32,
                            htlc.cltv_expiry as i64,
                        )?;
                    }

                    Ok(inv)
                })?;

                self.invoice_broadcast.send(inv)?;

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
                    fee_paid_msat.unwrap_or_default() as i64,
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
                let mut height = self.channel_manager.current_best_block().height;

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
            Event::PaymentForwarded {
                prev_channel_id,
                next_channel_id,
                prev_user_channel_id: _,
                next_user_channel_id: _,
                total_fee_earned_msat,
                skimmed_fee_msat: _,
                claim_from_onchain_tx,
                outbound_amount_forwarded_msat,
            } => {
                if claim_from_onchain_tx
                    || total_fee_earned_msat.is_none()
                    || outbound_amount_forwarded_msat.is_none()
                {
                    return Ok(());
                }

                let prev_channel_id = prev_channel_id.expect("safe after ldk 0.0.107");
                let next_channel_id = next_channel_id.expect("safe after ldk 0.0.107");

                let channels = self.channel_manager.list_channels();
                let prev_scid = channels
                    .iter()
                    .find(|c| c.channel_id == prev_channel_id)
                    .and_then(|c| c.short_channel_id)
                    .ok_or(anyhow!("Could not find prev channel"))?;
                let next_scid = channels
                    .iter()
                    .find(|c| c.channel_id == next_channel_id)
                    .and_then(|c| c.short_channel_id)
                    .ok_or(anyhow!("Could not find next channel"))?;

                log_debug!(self.logger, "EVENT: PaymentForwarded, prev_channel_id: {prev_channel_id:?}, next_channel_id: {next_channel_id:?}, total_fee_earned_msat: {total_fee_earned_msat:?}, outbound_amount_forwarded_msat: {outbound_amount_forwarded_msat:?}");

                let mut conn = self.db_pool.get()?;
                RoutedPayment::create(
                    &mut conn,
                    prev_channel_id.0.to_vec(),
                    prev_scid as i64,
                    next_channel_id.0.to_vec(),
                    next_scid as i64,
                    total_fee_earned_msat.unwrap() as i64,
                    outbound_amount_forwarded_msat.unwrap() as i64,
                )?;

                Ok(())
            }
            Event::ChannelPending {
                channel_id,
                user_channel_id,
                former_temporary_channel_id: _,
                counterparty_node_id,
                funding_txo,
                channel_type,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: ChannelPending channel_id: {channel_id}, user_channel_id: {user_channel_id}, counterparty_node_id: {counterparty_node_id}, channel_type: {channel_type:?}");

                let mut conn = self.db_pool.get()?;
                conn.transaction::<_, anyhow::Error, _>(|conn| {
                    Channel::mark_success(
                        conn,
                        user_channel_id as i32,
                        funding_txo.to_string(),
                        channel_id.0.to_vec(),
                    )?;
                    Ok(())
                })?;

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
                if let Ok(Some(_)) = Channel::find_by_id(&mut conn, id) {
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

                match reason {
                    ClosureReason::FundingBatchClosure
                    | ClosureReason::DisconnectedPeer
                    | ClosureReason::CounterpartyCoopClosedUnfundedChannel => {
                        log_debug!(self.logger, "EVENT: ChannelClosed, ignored");
                    }
                    reason => {
                        log_debug!(self.logger, "EVENT: ChannelClosed persisting to db");
                        ChannelClosure::create(
                            &mut conn,
                            id,
                            node_id,
                            channel_funding_txo.map(|x| x.into_bitcoin_outpoint()),
                            reason.to_string(),
                        )?;
                    }
                }
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
                funding_satoshis,
                push_msat,
                channel_type: _,
            } => {
                log_debug!(
                    self.logger,
                    "EVENT: OpenChannelRequest incoming: {counterparty_node_id}"
                );

                // todo get zero conf peers from config
                let mut trust_zero_conf = false;

                let lock = self.channel_acceptor.read().await;

                match lock.as_ref() {
                    None => log_trace!(
                        self.logger,
                        "No channel acceptor registered, auto accepting channel"
                    ),
                    Some(channel_acceptor) => {
                        let chain_hash = ChainHash::using_genesis_block(self.wallet.network);

                        let mut listener =
                            channel_acceptor.add_listener(temporary_channel_id).await;

                        let request = ChannelAcceptorRequest {
                            node_pubkey: counterparty_node_id,
                            chain_hash: chain_hash.encode(),
                            pending_chan_id: temporary_channel_id,
                            funding_amt: funding_satoshis,
                            push_amt: push_msat,
                            // todo https://github.com/lightningdevkit/rust-lightning/pull/3019
                            dust_limit: 0,
                            max_value_in_flight: 0,
                            channel_reserve: 0,
                            min_htlc: 0,
                            fee_per_kw: 0,
                            csv_delay: 0,
                            max_accepted_htlcs: 0,
                            channel_flags: 0,
                            commitment_type: 0,
                            wants_zero_conf: false,
                            wants_scid_alias: false,
                        };

                        channel_acceptor.send_request(request)?;
                        drop(lock);

                        let msg = listener.recv().await;

                        match msg {
                            None => {
                                log_error!(self.logger, "Channel acceptor timed out");
                                if let Err(e) =
                                    self.channel_manager.force_close_without_broadcasting_txn(
                                        &temporary_channel_id,
                                        &counterparty_node_id,
                                        "Channel acceptor timed out".to_string(),
                                    )
                                {
                                    log_error!(self.logger, "Error closing channel: {e:?}");
                                }
                                return Ok(());
                            }
                            Some(response) => {
                                log_info!(self.logger, "Channel acceptor response: {response:?}");
                                debug_assert_eq!(response.pending_chan_id, temporary_channel_id);

                                if !response.accept {
                                    let reason = if response.error.is_empty() {
                                        "Channel acceptor declined channel, closing channel"
                                            .to_string()
                                    } else {
                                        response.error
                                    };

                                    log_info!(
                                        self.logger,
                                        "Channel acceptor declined channel, closing channel for reason: {reason}"
                                    );
                                    if let Err(e) =
                                        self.channel_manager.force_close_without_broadcasting_txn(
                                            &temporary_channel_id,
                                            &counterparty_node_id,
                                            reason,
                                        )
                                    {
                                        log_error!(self.logger, "Error closing channel: {e:?}");
                                    }
                                }

                                trust_zero_conf |= response.zero_conf;
                            }
                        }
                    }
                }

                // save params to db
                let mut conn = self.db_pool.get()?;
                let params = Channel::create(
                    &mut conn,
                    counterparty_node_id.encode(),
                    None,
                    push_msat as i64,
                    false, // todo set private correctly
                    false,
                    funding_satoshis as i64,
                    trust_zero_conf,
                )?;

                let result = if trust_zero_conf {
                    self.channel_manager
                        .accept_inbound_channel_from_trusted_peer_0conf(
                            &temporary_channel_id,
                            &counterparty_node_id,
                            params.id as u128,
                        )
                } else {
                    self.channel_manager.accept_inbound_channel(
                        &temporary_channel_id,
                        &counterparty_node_id,
                        params.id as u128,
                    )
                };

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
            }
            Event::InvoiceReceived {
                payment_id,
                invoice,
                context,
                responder,
            } => {
                log_debug!(self.logger, "EVENT: InvoiceReceived, payment_id: {payment_id:?}, invoice: {invoice:?}, context: {context:?}, responder: {responder:?}");
                self.channel_manager
                    .send_payment_for_bolt12_invoice(&invoice, &context)
                    .map_err(|e| anyhow!("ERROR: Failed to send payment for invoice: {e:?}"))?;
                Ok(())
            }
            Event::OnionMessageIntercepted { .. } => {
                log_debug!(self.logger, "EVENT: OnionMessageIntercepted");
                Ok(())
            }
            Event::OnionMessagePeerConnected { .. } => {
                log_debug!(self.logger, "EVENT: OnionMessagePeerConnected");
                Ok(())
            }
        }
    }
}
