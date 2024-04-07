#![allow(deprecated)]
#![allow(unused)]

use crate::models::invoice::InvoiceStatus;
use crate::models::CreatedInvoice;
use crate::node::Node;
use crate::proto::channel_point::FundingTxid;
use crate::proto::invoice::InvoiceState;
use crate::proto::lightning_server::Lightning;
use crate::proto::pending_channels_response::{PendingChannel, PendingOpenChannel};
use crate::proto::*;
use bdk::chain::ConfirmationTime;
use bitcoin::address::Payload;
use bitcoin::consensus::serialize;
use bitcoin::ecdsa::Signature;
use bitcoin::hashes::{sha256::Hash as Sha256, sha256d::Hash as Sha256d, Hash};
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use bitcoin::secp256k1::{Message, PublicKey};
use bitcoin::{Address, FeeRate, Network, ScriptBuf, TxOut};
use bitcoincore_rpc::RpcApi;
use itertools::Itertools;
use lightning::ln::channelmanager::{PaymentId, Retry};
use lightning::sign::{NodeSigner, Recipient};
use lightning::util::ser::Writeable;
use lightning_invoice::payment::{
    payment_parameters_from_invoice, payment_parameters_from_zero_amount_invoice,
};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

#[tonic::async_trait]
impl Lightning for Node {
    async fn wallet_balance(
        &self,
        _request: Request<WalletBalanceRequest>,
    ) -> Result<Response<WalletBalanceResponse>, Status> {
        let balance = self.wallet.balance();

        let account = WalletAccountBalance {
            confirmed_balance: balance.trusted_spendable() as i64,
            unconfirmed_balance: balance.untrusted_pending as i64,
        };

        let mut account_balance = HashMap::with_capacity(1);
        account_balance.insert("default".to_string(), account);

        let response = WalletBalanceResponse {
            total_balance: balance.total() as i64,
            confirmed_balance: balance.trusted_spendable() as i64,
            unconfirmed_balance: balance.untrusted_pending as i64,
            locked_balance: 0,
            reserved_balance_anchor_chan: 0,
            account_balance,
        };

        Ok(Response::new(response))
    }

    async fn channel_balance(
        &self,
        _: Request<ChannelBalanceRequest>,
    ) -> Result<Response<ChannelBalanceResponse>, Status> {
        let channels = self.channel_manager.list_channels();

        let balance = channels
            .iter()
            .filter(|c| c.is_channel_ready)
            .map(|c| c.balance_msat)
            .sum::<u64>();

        let remote_balance = channels
            .iter()
            .filter(|c| c.is_channel_ready)
            .map(|c| c.channel_value_satoshis - (c.balance_msat / 1000))
            .sum::<u64>();

        let pending_open_balance = channels
            .iter()
            .filter(|c| !c.is_channel_ready)
            .map(|c| c.balance_msat)
            .sum::<u64>();

        let pending_remote_balance = channels
            .iter()
            .filter(|c| !c.is_channel_ready)
            .map(|c| c.channel_value_satoshis - (c.balance_msat / 1000))
            .sum::<u64>();

        let local_balance = Amount {
            sat: balance / 1000,
            msat: balance,
        };

        let remote_balance = Amount {
            sat: remote_balance / 1000,
            msat: remote_balance,
        };

        let pending_local_balance = Amount {
            sat: pending_open_balance / 1000,
            msat: pending_open_balance,
        };

        let pending_remote_balance = Amount {
            sat: pending_remote_balance / 1000,
            msat: pending_remote_balance,
        };

        let response = ChannelBalanceResponse {
            balance: (balance / 1000) as i64,
            pending_open_balance: (pending_open_balance / 1000) as i64,
            local_balance: Some(local_balance),
            remote_balance: Some(remote_balance),
            unsettled_local_balance: Some(Amount::default()),
            unsettled_remote_balance: Some(Amount::default()),
            pending_open_local_balance: Some(pending_local_balance),
            pending_open_remote_balance: Some(pending_remote_balance),
        };

        Ok(Response::new(response))
    }

    async fn get_transactions(
        &self,
        request: Request<GetTransactionsRequest>,
    ) -> Result<Response<TransactionDetails>, Status> {
        let GetTransactionsRequest {
            start_height,
            end_height,
            account,
        } = request.into_inner();

        if !account.is_empty() || account != "default" {
            return Err(Status::unimplemented("account is not supported"));
        }

        let transactions = self
            .wallet
            .list_transactions()
            .map_err(|e| Status::internal(e.to_string()))?;

        let current_height = self
            .bitcoind
            .get_block_count()
            .map_err(|e| Status::internal(e.to_string()))?;

        let transactions = transactions
            .into_iter()
            .filter_map(|t| {
                let (block_height, time_stamp) = match t.confirmation_time {
                    ConfirmationTime::Confirmed { height, time } => (height as i32, time),
                    ConfirmationTime::Unconfirmed { last_seen } => (-1, last_seen),
                };

                if block_height < start_height || block_height > end_height {
                    return None;
                }

                let raw_tx_hex = hex::encode(serialize(&t.transaction));

                let block_hash = if block_height > 0 {
                    self.bitcoind
                        .get_block_hash(block_height as u64)
                        .unwrap()
                        .to_string()
                } else {
                    String::new()
                };

                let dest_addresses = t
                    .transaction
                    .output
                    .iter()
                    .flat_map(|o| {
                        Address::from_script(&o.script_pubkey, self.network)
                            .map(|a| a.to_string())
                            .ok()
                    })
                    .collect();

                let output_details = t
                    .output_details
                    .iter()
                    .map(|o| {
                        let output_type = get_output_type(&o.spk);
                        OutputDetail {
                            output_type: output_type.into(),
                            address: o.address.clone().unwrap_or_default(),
                            pk_script: o.spk.to_hex_string(),
                            output_index: o.output_index as i64,
                            amount: o.amount as i64,
                            is_our_address: o.is_our_address,
                        }
                    })
                    .collect();

                let previous_outpoints = t
                    .previous_outpoints
                    .into_iter()
                    .map(|o| PreviousOutPoint {
                        outpoint: o.outpoint.to_string(),
                        is_our_output: o.is_our_output,
                    })
                    .collect_vec();

                Some(Transaction {
                    tx_hash: t.txid.to_string(),
                    amount: (t.received - t.sent) as i64,
                    num_confirmations: current_height as i32 - block_height + 1,
                    block_hash,
                    block_height,
                    time_stamp: time_stamp as i64,
                    total_fees: t.fee.unwrap_or_default() as i64,
                    dest_addresses,
                    output_details,
                    raw_tx_hex,
                    label: "".to_string(),
                    previous_outpoints,
                })
            })
            .collect();

        Ok(Response::new(TransactionDetails { transactions }))
    }

    async fn estimate_fee(
        &self,
        request: Request<EstimateFeeRequest>,
    ) -> Result<Response<EstimateFeeResponse>, Status> {
        let req = request.into_inner();

        let res = self
            .bitcoind
            .estimate_smart_fee(req.target_conf as u16, None)
            .map_err(|e| Status::internal(e.to_string()))?;

        if let Some(per_kb) = res.fee_rate {
            let fee_rate = FeeRate::from_sat_per_vb_unchecked(per_kb.to_sat() / 1_000);

            let outputs = req
                .addr_to_amount
                .into_iter()
                .map(|(addr, amt)| {
                    Address::from_str(&addr)
                        .map_err(|e| Status::invalid_argument(format!("{:?}", e)))
                        .map(|a| TxOut {
                            script_pubkey: a.payload.script_pubkey(),
                            value: amt as u64,
                        })
                })
                .collect::<Result<Vec<TxOut>, Status>>()?;

            let fee_sat = self
                .wallet
                .estimate_fee_to_outputs(outputs, Some(fee_rate))
                .map_err(|e| Status::internal(e.to_string()))?;
            let response = EstimateFeeResponse {
                fee_sat: fee_sat as i64,
                feerate_sat_per_byte: fee_rate.to_sat_per_vb_ceil() as i64,
                sat_per_vbyte: fee_rate.to_sat_per_vb_ceil(),
            };

            Ok(Response::new(response))
        } else {
            Err(Status::internal("Error getting fee rate"))
        }
    }

    async fn send_coins(
        &self,
        request: Request<SendCoinsRequest>,
    ) -> Result<Response<SendCoinsResponse>, Status> {
        let req = request.into_inner();

        if req.sat_per_byte != 0 {
            return Err(Status::unimplemented("sat_per_byte is not supported"));
        }

        if req.sat_per_vbyte != 0 && req.target_conf != 0 {
            return Err(Status::invalid_argument(
                "Cannot have both sat_per_vbyte and target_conf",
            ));
        }

        if req.amount == 0 && !req.send_all {
            return Err(Status::invalid_argument("amount or send_all is required"));
        }

        if req.min_confs != 0 {
            return Err(Status::unimplemented("min_confs is not supported"));
        }

        let address = Address::from_str(&req.addr)
            .map_err(|e| Status::invalid_argument(format!("{:?}", e)))?
            .require_network(self.network)
            .map_err(|e| Status::invalid_argument(format!("{:?}", e)))?;

        let fee_rate = if req.sat_per_vbyte != 0 {
            Some(FeeRate::from_sat_per_vb_unchecked(req.sat_per_vbyte))
        } else if req.target_conf != 0 {
            let res = self
                .bitcoind
                .estimate_smart_fee(req.target_conf as u16, None)
                .map_err(|e| Status::internal(e.to_string()))?;
            if let Some(per_kb) = res.fee_rate {
                Some(FeeRate::from_sat_per_vb_unchecked(per_kb.to_sat() / 1_000))
            } else {
                return Err(Status::internal("Error getting fee rate"));
            }
        } else {
            None
        };

        let txid = if req.amount > 0 {
            self.wallet
                .send_to_address(address, req.amount as u64, fee_rate)
                .map_err(|e| Status::internal(format!("Error sending coins: {:?}", e)))?
        } else if req.send_all {
            self.wallet
                .sweep(address, fee_rate)
                .map_err(|e| Status::internal(format!("Error sending coins: {:?}", e)))?
        } else {
            return Err(Status::invalid_argument("amount or send_all is required"));
        };

        Ok(Response::new(SendCoinsResponse {
            txid: txid.to_string(),
        }))
    }

    async fn list_unspent(
        &self,
        request: Request<ListUnspentRequest>,
    ) -> Result<Response<ListUnspentResponse>, Status> {
        let ListUnspentRequest {
            min_confs,
            max_confs,
            account,
        } = request.into_inner();

        if !account.is_empty() || account != "default" {
            return Err(Status::unimplemented("account is not supported"));
        }

        let utxos = self
            .wallet
            .list_utxos()
            .map_err(|e| Status::internal(e.to_string()))?;

        let current_height = self
            .bitcoind
            .get_block_count()
            .map_err(|e| Status::internal(e.to_string()))?;

        let utxos = utxos
            .into_iter()
            .filter_map(|u| {
                let confs = match u.confirmation_time {
                    ConfirmationTime::Confirmed { height, .. } => {
                        current_height as i32 - height as i32 + 1
                    }
                    ConfirmationTime::Unconfirmed { .. } => 0,
                };

                if confs >= min_confs && confs <= max_confs {
                    let address_type = if u.txout.script_pubkey.is_v1_p2tr() {
                        AddressType::TaprootPubkey
                    } else if u.txout.script_pubkey.is_v0_p2wpkh() {
                        AddressType::WitnessPubkeyHash
                    } else if u.txout.script_pubkey.is_p2sh() {
                        AddressType::NestedPubkeyHash
                    } else {
                        return None; // unsupported address type
                    };
                    let address = Address::from_script(&u.txout.script_pubkey, self.network)
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    Some(Utxo {
                        address_type: address_type.into(),
                        address,
                        amount_sat: u.txout.value as i64,
                        pk_script: u.txout.script_pubkey.to_hex_string(),
                        outpoint: Some(OutPoint {
                            txid_bytes: u.outpoint.txid.encode(),
                            txid_str: u.outpoint.txid.to_string(),
                            output_index: u.outpoint.vout,
                        }),
                        confirmations: confs as i64,
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(Response::new(ListUnspentResponse { utxos }))
    }

    type SubscribeTransactionsStream = ReceiverStream<Result<Transaction, Status>>;

    async fn subscribe_transactions(
        &self,
        request: Request<GetTransactionsRequest>,
    ) -> Result<Response<Self::SubscribeTransactionsStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn send_many(
        &self,
        request: Request<SendManyRequest>,
    ) -> Result<Response<SendManyResponse>, Status> {
        let req = request.into_inner();

        if req.sat_per_byte != 0 {
            return Err(Status::unimplemented("sat_per_byte is not supported"));
        }

        if req.sat_per_vbyte != 0 && req.target_conf != 0 {
            return Err(Status::invalid_argument(
                "Cannot have both sat_per_vbyte and target_conf",
            ));
        }

        if req.addr_to_amount.is_empty() {
            return Err(Status::invalid_argument("addr_to_amount is required"));
        }

        if req.min_confs != 0 {
            return Err(Status::unimplemented("min_confs is not supported"));
        }

        let fee_rate = if req.sat_per_vbyte != 0 {
            Some(FeeRate::from_sat_per_vb_unchecked(req.sat_per_vbyte))
        } else if req.target_conf != 0 {
            let res = self
                .bitcoind
                .estimate_smart_fee(req.target_conf as u16, None)
                .map_err(|e| Status::internal(e.to_string()))?;
            if let Some(per_kb) = res.fee_rate {
                Some(FeeRate::from_sat_per_vb_unchecked(per_kb.to_sat() / 1_000))
            } else {
                return Err(Status::internal("Error getting fee rate"));
            }
        } else {
            None
        };

        let outputs = req
            .addr_to_amount
            .into_iter()
            .map(|(addr, amt)| {
                Address::from_str(&addr)
                    .map_err(|e| Status::invalid_argument(format!("{:?}", e)))
                    .map(|a| TxOut {
                        script_pubkey: a.payload.script_pubkey(),
                        value: amt as u64,
                    })
            })
            .collect::<Result<Vec<TxOut>, Status>>()?;

        let txid = self
            .wallet
            .send_to_outputs(outputs, fee_rate)
            .map_err(|e| Status::internal(format!("Error sending coins: {:?}", e)))?;

        Ok(Response::new(SendManyResponse {
            txid: txid.to_string(),
        }))
    }

    async fn new_address(
        &self,
        _: Request<NewAddressRequest>,
    ) -> Result<Response<NewAddressResponse>, Status> {
        let address_info = self
            .wallet
            .get_new_address()
            .map_err(|e| Status::internal(e.to_string()))?;

        let response = NewAddressResponse {
            address: address_info.address.to_string(),
        };

        Ok(Response::new(response))
    }

    async fn sign_message(
        &self,
        request: Request<SignMessageRequest>,
    ) -> Result<Response<SignMessageResponse>, Status> {
        let req = request.into_inner();

        let signature = lightning::util::message_signing::sign(
            &req.msg,
            &self.keys_manager.get_node_secret_key(),
        )
        .map_err(|e| Status::internal(e.to_string()))?;
        let response = SignMessageResponse { signature };
        Ok(Response::new(response))
    }

    async fn verify_message(
        &self,
        request: Request<VerifyMessageRequest>,
    ) -> Result<Response<VerifyMessageResponse>, Status> {
        let req = request.into_inner();

        match lightning::util::message_signing::recover_pk(&req.msg, &req.signature) {
            Ok(pubkey) => {
                let graph = self.network_graph.read_only();
                let valid = graph
                    .node(&pubkey.into())
                    .map(|n| !n.channels.is_empty())
                    .unwrap_or_default();
                let response = VerifyMessageResponse {
                    valid,
                    pubkey: pubkey.to_string(),
                };
                Ok(Response::new(response))
            }
            Err(_) => {
                let response = VerifyMessageResponse {
                    valid: false,
                    pubkey: "".to_string(),
                };
                Ok(Response::new(response))
            }
        }
    }

    async fn connect_peer(
        &self,
        request: Request<ConnectPeerRequest>,
    ) -> Result<Response<ConnectPeerResponse>, Status> {
        let req = request.into_inner();

        if let Some(addr) = req.addr {
            let pk = PublicKey::from_str(&addr.pubkey)
                .map_err(|e| Status::invalid_argument(format!("{:?}", e)))?;

            let socket = SocketAddr::from_str(&addr.host)
                .map_err(|e| Status::invalid_argument(format!("{:?}", e)))?;

            self.connect_to_peer(pk, socket)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
        } else {
            return Err(Status::invalid_argument("addr is required"));
        }

        Ok(Response::new(ConnectPeerResponse {}))
    }

    async fn disconnect_peer(
        &self,
        request: Request<DisconnectPeerRequest>,
    ) -> Result<Response<DisconnectPeerResponse>, Status> {
        let req = request.into_inner();
        let pk = PublicKey::from_str(&req.pub_key).unwrap();
        self.peer_manager.disconnect_by_node_id(pk);

        Ok(Response::new(DisconnectPeerResponse {}))
    }

    async fn list_peers(
        &self,
        _: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        let peers = self.peer_manager.get_peer_node_ids();

        let peers = peers
            .into_iter()
            .map(|(p, addr)| Peer {
                pub_key: p.to_string(),
                address: addr.map(|a| a.to_string()).unwrap_or_default(),
                bytes_sent: 0,
                bytes_recv: 0,
                sat_sent: 0,
                sat_recv: 0,
                inbound: false,
                ping_time: 0,
                sync_type: 0,
                features: Default::default(),
                errors: vec![],
                flap_count: 0,
                last_flap_ns: 0,
                last_ping_payload: vec![],
            })
            .collect();

        let resp = ListPeersResponse { peers };

        Ok(Response::new(resp))
    }

    type SubscribePeerEventsStream = ReceiverStream<Result<PeerEvent, Status>>;

    async fn subscribe_peer_events(
        &self,
        request: Request<PeerEventSubscription>,
    ) -> Result<Response<Self::SubscribePeerEventsStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn get_info(
        &self,
        _: Request<GetInfoRequest>,
    ) -> Result<Response<GetInfoResponse>, Status> {
        let num_peers = self.peer_manager.get_peer_node_ids().len() as u32;
        let channels = self.channel_manager.list_channels();
        let best_block = self.channel_manager.current_best_block();

        let chain = Chain {
            chain: "bitcoin".to_string(),
            network: self.network.to_string(),
        };

        let resp = GetInfoResponse {
            version: "0.1.0".to_string(),
            commit_hash: "unknown".to_string(),
            identity_pubkey: self
                .keys_manager
                .get_node_id(Recipient::Node)
                .expect("this is safe")
                .to_string(),
            alias: "".to_string(),
            color: "#000000".to_string(),
            num_pending_channels: channels.iter().filter(|c| !c.is_channel_ready).count() as u32,
            num_active_channels: channels.iter().filter(|c| c.is_usable).count() as u32,
            num_inactive_channels: channels.iter().filter(|c| !c.is_usable).count() as u32,
            num_peers,
            block_height: best_block.height(),
            block_hash: best_block.block_hash().to_string(),
            best_header_timestamp: 0,
            synced_to_chain: true, // idk if correct
            synced_to_graph: true, // idk if correct
            testnet: self.network != Network::Bitcoin,
            chains: vec![chain],
            uris: vec![],
            features: Default::default(), // todo
            require_htlc_interceptor: false,
            store_final_htlc_resolutions: false,
        };

        Ok(Response::new(resp))
    }

    async fn get_recovery_info(
        &self,
        _: Request<GetRecoveryInfoRequest>,
    ) -> Result<Response<GetRecoveryInfoResponse>, Status> {
        let resp = GetRecoveryInfoResponse {
            recovery_mode: false,
            recovery_finished: false,
            progress: 0.0,
        };
        Ok(Response::new(resp))
    }

    async fn pending_channels(
        &self,
        _: Request<PendingChannelsRequest>,
    ) -> Result<Response<PendingChannelsResponse>, Status> {
        let channels = self.channel_manager.list_channels();
        let limbo: u64 = channels
            .iter()
            .filter(|c| !c.is_channel_ready)
            .map(|c| c.balance_msat)
            .sum();
        let pending_open_channels = channels
            .into_iter()
            .filter(|c| !c.is_channel_ready)
            .map(|c| PendingOpenChannel {
                channel: Some(PendingChannel {
                    remote_node_pub: c.counterparty.node_id.to_string(),
                    channel_point: c.funding_txo.map(|t| t.to_string()).unwrap_or_default(),
                    capacity: c.channel_value_satoshis as i64,
                    local_balance: (c.balance_msat / 1_000) as i64,
                    remote_balance: (c.channel_value_satoshis - (c.balance_msat / 1_000)) as i64,
                    local_chan_reserve_sat: c.unspendable_punishment_reserve.unwrap_or_default()
                        as i64,
                    remote_chan_reserve_sat: c.counterparty.unspendable_punishment_reserve as i64,
                    initiator: if c.is_outbound { 1 } else { 2 },
                    commitment_type: CommitmentType::StaticRemoteKey.into(), // todo handle anchors
                    num_forwarding_packages: 0,
                    chan_status_flags: "".to_string(),
                    private: !c.is_public,
                    memo: "".to_string(),
                }),
                commit_fee: 0,
                commit_weight: 0,
                fee_per_kw: 0,
                funding_expiry_blocks: 0,
            })
            .collect();

        let resp = PendingChannelsResponse {
            total_limbo_balance: (limbo / 1_000) as i64,
            pending_open_channels,
            pending_closing_channels: vec![],
            pending_force_closing_channels: vec![],
            waiting_close_channels: vec![],
        };

        Ok(Response::new(resp))
    }

    async fn list_channels(
        &self,
        request: Request<ListChannelsRequest>,
    ) -> Result<Response<ListChannelsResponse>, Status> {
        let chans = self.channel_manager.list_channels();
        let channels = chans
            .into_iter()
            .map(|c| Channel {
                active: c.is_usable,
                remote_pubkey: c.counterparty.node_id.to_string(),
                channel_point: c.funding_txo.map(|t| t.to_string()).unwrap_or_default(),
                chan_id: c.short_channel_id.unwrap_or_default(),
                capacity: c.channel_value_satoshis as i64,
                local_balance: c.balance_msat as i64 / 1_000,
                remote_balance: c
                    .counterparty
                    .outbound_htlc_maximum_msat
                    .unwrap_or_default() as i64
                    / 1_000,
                commit_fee: 0,
                commit_weight: 0,
                fee_per_kw: c.feerate_sat_per_1000_weight.unwrap_or_default() as i64,
                unsettled_balance: 0,
                total_satoshis_sent: 0,
                total_satoshis_received: 0,
                num_updates: 0,
                pending_htlcs: vec![],
                csv_delay: c.force_close_spend_delay.unwrap_or_default() as u32,
                private: !c.is_public,
                initiator: c.is_outbound,
                chan_status_flags: "".to_string(),
                local_chan_reserve_sat: c.unspendable_punishment_reserve.unwrap_or_default() as i64,
                remote_chan_reserve_sat: c.counterparty.unspendable_punishment_reserve as i64,
                static_remote_key: true,
                commitment_type: 0,
                lifetime: 0,
                uptime: 0,
                close_address: "".to_string(),
                push_amount_sat: 0,
                thaw_height: 0,
                local_constraints: None,
                remote_constraints: None,
                alias_scids: c.inbound_scid_alias.map(|a| vec![a]).unwrap_or_default(),
                zero_conf: false,
                zero_conf_confirmed_scid: 0,
                peer_alias: "".to_string(),
                peer_scid_alias: c.outbound_scid_alias.unwrap_or_default(),
                memo: "".to_string(),
            })
            .collect();

        let resp = ListChannelsResponse { channels };
        Ok(Response::new(resp))
    }

    type SubscribeChannelEventsStream = ReceiverStream<Result<ChannelEventUpdate, Status>>;

    async fn subscribe_channel_events(
        &self,
        request: Request<ChannelEventSubscription>,
    ) -> Result<Response<Self::SubscribeChannelEventsStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn closed_channels(
        &self,
        request: Request<ClosedChannelsRequest>,
    ) -> Result<Response<ClosedChannelsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn open_channel_sync(
        &self,
        request: Request<OpenChannelRequest>,
    ) -> Result<Response<ChannelPoint>, Status> {
        let req = request.into_inner();

        let pk = if req.node_pubkey_string.is_empty() {
            PublicKey::from_slice(&req.node_pubkey)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?
        } else {
            PublicKey::from_str(&req.node_pubkey_string)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?
        };

        let fee_rate = if req.sat_per_vbyte > 0 {
            Some(req.sat_per_vbyte as i32)
        } else {
            None
        };

        // todo handle private channels
        let outpoint = self
            .open_channel_with_timeout(
                pk,
                req.local_funding_amount as u64,
                req.push_sat as u64 * 1_000,
                fee_rate,
                30,
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let resp = ChannelPoint {
            output_index: outpoint.vout,
            funding_txid: Some(FundingTxid::FundingTxidStr(outpoint.txid.to_string())),
        };

        Ok(Response::new(resp))
    }

    type OpenChannelStream = ReceiverStream<Result<OpenStatusUpdate, Status>>;

    async fn open_channel(
        &self,
        request: Request<OpenChannelRequest>,
    ) -> Result<Response<Self::OpenChannelStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn batch_open_channel(
        &self,
        request: Request<BatchOpenChannelRequest>,
    ) -> Result<Response<BatchOpenChannelResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn funding_state_step(
        &self,
        request: Request<FundingTransitionMsg>,
    ) -> Result<Response<FundingStateStepResp>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type ChannelAcceptorStream = ReceiverStream<Result<ChannelAcceptRequest, Status>>;

    async fn channel_acceptor(
        &self,
        request: Request<Streaming<ChannelAcceptResponse>>,
    ) -> Result<Response<Self::ChannelAcceptorStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type CloseChannelStream = ReceiverStream<Result<CloseStatusUpdate, Status>>;

    async fn close_channel(
        &self,
        request: Request<CloseChannelRequest>,
    ) -> Result<Response<Self::CloseChannelStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn abandon_channel(
        &self,
        request: Request<AbandonChannelRequest>,
    ) -> Result<Response<AbandonChannelResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type SendPaymentStream = ReceiverStream<Result<SendResponse, Status>>;

    async fn send_payment(
        &self,
        request: Request<Streaming<SendRequest>>,
    ) -> Result<Response<Self::SendPaymentStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn send_payment_sync(
        &self,
        request: Request<SendRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        let req = request.into_inner();

        if req.amt > 0 && req.amt_msat > 0 {
            return Err(Status::invalid_argument("Cannot have amt and amt_msat"));
        }

        if let Ok(invoice) = Bolt11Invoice::from_str(&req.payment_request) {
            let amount_msats = if req.amt_msat > 0 {
                Some(req.amt_msat as u64)
            } else if req.amt > 0 {
                Some(req.amt as u64 * 1_000)
            } else {
                None
            };

            if invoice
                .amount_milli_satoshis()
                .is_some_and(|a| a != amount_msats.unwrap_or(a))
            {
                return Err(Status::invalid_argument(
                    "Invoice amount does not match request",
                ));
            }

            let payment_hash = invoice.payment_hash().as_byte_array().to_vec();
            let response: SendResponse = match self
                .pay_invoice_with_timeout(invoice, amount_msats, Some(5 * 60)) // default 5 minute timeout
                .await
            {
                Ok(payment) => {
                    let hops: Vec<Hop> = payment
                        .path()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|hop| Hop {
                            chan_id: hop.short_channel_id,
                            chan_capacity: 0,
                            amt_to_forward: 0,
                            fee: (hop.fee_msat / 1_000) as i64,
                            expiry: hop.cltv_expiry_delta,
                            amt_to_forward_msat: 0,
                            fee_msat: hop.fee_msat as i64,
                            pub_key: hop.pubkey.to_string(),
                            tlv_payload: false,
                            mpp_record: None,
                            amp_record: None,
                            custom_records: Default::default(),
                            metadata: vec![],
                        })
                        .collect();

                    let total_time_lock = hops.iter().map(|h| h.expiry).sum::<u32>();

                    let payment_route = Route {
                        total_time_lock,
                        total_fees: payment.fee_msats.unwrap_or_default() as i64 / 1_000,
                        total_amt: payment.amount_msats as i64 / 1_000,
                        hops,
                        total_fees_msat: payment.amount_msats as i64,
                        total_amt_msat: payment.amount_msats as i64,
                    };

                    SendResponse {
                        payment_error: "".to_string(),
                        payment_preimage: payment
                            .preimage()
                            .map(|p| p.to_vec())
                            .unwrap_or_default(),
                        payment_route: None,
                        payment_hash,
                    }
                }
                Err(e) => SendResponse {
                    payment_error: e.to_string(),
                    payment_preimage: vec![],
                    payment_route: None,
                    payment_hash,
                },
            };

            return Ok(Response::new(response));
        }

        Err(Status::unimplemented(
            "only paying invoices is supported for now",
        ))
    }

    type SendToRouteStream = ReceiverStream<Result<SendResponse, Status>>;

    async fn send_to_route(
        &self,
        request: Request<Streaming<SendToRouteRequest>>,
    ) -> Result<Response<Self::SendToRouteStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn send_to_route_sync(
        &self,
        request: Request<SendToRouteRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn add_invoice(
        &self,
        request: Request<Invoice>,
    ) -> Result<Response<AddInvoiceResponse>, Status> {
        let req = request.into_inner();

        let expiry = if req.expiry == 0 {
            Some(86_400)
        } else {
            Some(req.expiry as u32)
        };

        let msats = if req.value_msat != 0 && req.value != 0 {
            return Err(Status::invalid_argument("Cannot have value and value_msat"));
        } else if req.value_msat == 0 && req.value == 0 {
            None
        } else if req.value_msat != 0 {
            Some(req.value_msat as u64)
        } else {
            Some(req.value as u64 * 1_000)
        };

        let CreatedInvoice { id, bolt11 } = if req.description_hash.is_empty() {
            let desc =
                Description::new(req.memo).map_err(|e| Status::invalid_argument(e.to_string()))?;
            let desc = Bolt11InvoiceDescription::Direct(&desc);

            self.create_invoice(desc, msats, expiry)
                .map_err(|e| Status::internal(e.to_string()))?
        } else {
            let hash = Sha256::from_slice(&req.description_hash)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let hash = lightning_invoice::Sha256(hash);
            let desc = Bolt11InvoiceDescription::Hash(&hash);

            self.create_invoice(desc, msats, expiry)
                .map_err(|e| Status::internal(e.to_string()))?
        };

        let response = AddInvoiceResponse {
            payment_request: bolt11.to_string(),
            r_hash: bolt11.payment_hash().to_byte_array().to_vec(),
            add_index: id as u64,
            payment_addr: bolt11.payment_secret().0.to_vec(),
        };

        Ok(Response::new(response))
    }

    async fn list_invoices(
        &self,
        request: Request<ListInvoiceRequest>,
    ) -> Result<Response<ListInvoiceResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn lookup_invoice(
        &self,
        request: Request<PaymentHash>,
    ) -> Result<Response<Invoice>, Status> {
        let req = request.into_inner();

        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(e.to_string()))?;
        let opt = crate::models::invoice::Invoice::find_by_payment_hash(&mut conn, &req.r_hash)
            .map_err(|e| Status::internal(e.to_string()))?;

        match opt {
            Some(invoice) => {
                let invoice = invoice_to_lnrpc_invoice(invoice);
                Ok(Response::new(invoice))
            }
            None => Err(Status::not_found("Invoice not found")),
        }
    }

    type SubscribeInvoicesStream = ReceiverStream<Result<Invoice, Status>>;

    async fn subscribe_invoices(
        &self,
        request: Request<InvoiceSubscription>,
    ) -> Result<Response<Self::SubscribeInvoicesStream>, Status> {
        let mut node_rx = self.invoice_broadcast.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Invoice, Status>>(128);
        tokio::spawn(async move {
            while let Ok(item) = node_rx.recv().await {
                if tx.is_closed() {
                    break;
                }

                let invoice = invoice_to_lnrpc_invoice(item);
                tx.send(Ok(invoice)).await.unwrap()
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn decode_pay_req(
        &self,
        request: Request<PayReqString>,
    ) -> Result<Response<PayReq>, Status> {
        let req = request.into_inner();

        let invoice = Bolt11Invoice::from_str(req.pay_req.as_str())
            .map_err(|e| Status::invalid_argument(format!("{:?}", e)))?;

        let (description, description_hash) = match invoice.description() {
            Bolt11InvoiceDescription::Direct(desc) => (desc.to_string(), String::new()),
            Bolt11InvoiceDescription::Hash(hash) => (String::new(), hash.0.to_string()),
        };

        let route_hints = invoice
            .route_hints()
            .iter()
            .map(|hint| RouteHint {
                hop_hints: hint
                    .0
                    .iter()
                    .map(|hop| HopHint {
                        node_id: hop.src_node_id.to_string(),
                        chan_id: hop.short_channel_id,
                        fee_base_msat: hop.fees.base_msat,
                        fee_proportional_millionths: hop.fees.proportional_millionths,
                        cltv_expiry_delta: hop.cltv_expiry_delta as u32,
                    })
                    .collect(),
            })
            .collect();

        let resp = PayReq {
            destination: invoice.recover_payee_pub_key().to_string(),
            payment_hash: invoice.payment_hash().to_string(),
            num_satoshis: invoice
                .amount_milli_satoshis()
                .map(|a| a / 1_000)
                .unwrap_or(0) as i64,
            timestamp: invoice
                .timestamp()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            expiry: invoice.expiry_time().as_secs() as i64,
            description,
            description_hash,
            fallback_addr: invoice
                .fallback_addresses()
                .first()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            cltv_expiry: invoice.min_final_cltv_expiry_delta() as i64,
            route_hints,
            payment_addr: invoice.payment_secret().0.to_vec(),
            num_msat: invoice.amount_milli_satoshis().unwrap_or_default() as i64,
            features: Default::default(),
        };

        Ok(Response::new(resp))
    }

    async fn list_payments(
        &self,
        request: Request<ListPaymentsRequest>,
    ) -> Result<Response<ListPaymentsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn delete_payment(
        &self,
        request: Request<DeletePaymentRequest>,
    ) -> Result<Response<DeletePaymentResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn delete_all_payments(
        &self,
        request: Request<DeleteAllPaymentsRequest>,
    ) -> Result<Response<DeleteAllPaymentsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn describe_graph(
        &self,
        request: Request<ChannelGraphRequest>,
    ) -> Result<Response<ChannelGraph>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn get_node_metrics(
        &self,
        request: Request<NodeMetricsRequest>,
    ) -> Result<Response<NodeMetricsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn get_chan_info(
        &self,
        request: Request<ChanInfoRequest>,
    ) -> Result<Response<ChannelEdge>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn get_node_info(
        &self,
        request: Request<NodeInfoRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn query_routes(
        &self,
        request: Request<QueryRoutesRequest>,
    ) -> Result<Response<QueryRoutesResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn get_network_info(
        &self,
        request: Request<NetworkInfoRequest>,
    ) -> Result<Response<NetworkInfo>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn stop_daemon(
        &self,
        request: Request<StopRequest>,
    ) -> Result<Response<StopResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type SubscribeChannelGraphStream = ReceiverStream<Result<GraphTopologyUpdate, Status>>;

    async fn subscribe_channel_graph(
        &self,
        request: Request<GraphTopologySubscription>,
    ) -> Result<Response<Self::SubscribeChannelGraphStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn debug_level(
        &self,
        request: Request<DebugLevelRequest>,
    ) -> Result<Response<DebugLevelResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn fee_report(
        &self,
        request: Request<FeeReportRequest>,
    ) -> Result<Response<FeeReportResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn update_channel_policy(
        &self,
        request: Request<PolicyUpdateRequest>,
    ) -> Result<Response<PolicyUpdateResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn forwarding_history(
        &self,
        request: Request<ForwardingHistoryRequest>,
    ) -> Result<Response<ForwardingHistoryResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn export_channel_backup(
        &self,
        request: Request<ExportChannelBackupRequest>,
    ) -> Result<Response<ChannelBackup>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn export_all_channel_backups(
        &self,
        request: Request<ChanBackupExportRequest>,
    ) -> Result<Response<ChanBackupSnapshot>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn verify_chan_backup(
        &self,
        request: Request<ChanBackupSnapshot>,
    ) -> Result<Response<VerifyChanBackupResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn restore_channel_backups(
        &self,
        request: Request<RestoreChanBackupRequest>,
    ) -> Result<Response<RestoreBackupResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type SubscribeChannelBackupsStream = ReceiverStream<Result<ChanBackupSnapshot, Status>>;

    async fn subscribe_channel_backups(
        &self,
        request: Request<ChannelBackupSubscription>,
    ) -> Result<Response<Self::SubscribeChannelBackupsStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn bake_macaroon(
        &self,
        request: Request<BakeMacaroonRequest>,
    ) -> Result<Response<BakeMacaroonResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn list_macaroon_i_ds(
        &self,
        request: Request<ListMacaroonIDsRequest>,
    ) -> Result<Response<ListMacaroonIDsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn delete_macaroon_id(
        &self,
        request: Request<DeleteMacaroonIdRequest>,
    ) -> Result<Response<DeleteMacaroonIdResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn list_permissions(
        &self,
        request: Request<ListPermissionsRequest>,
    ) -> Result<Response<ListPermissionsResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn check_macaroon_permissions(
        &self,
        request: Request<CheckMacPermRequest>,
    ) -> Result<Response<CheckMacPermResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type RegisterRPCMiddlewareStream = ReceiverStream<Result<RpcMiddlewareRequest, Status>>;

    async fn register_rpc_middleware(
        &self,
        request: Request<Streaming<RpcMiddlewareResponse>>,
    ) -> Result<Response<Self::RegisterRPCMiddlewareStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn send_custom_message(
        &self,
        request: Request<SendCustomMessageRequest>,
    ) -> Result<Response<SendCustomMessageResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    type SubscribeCustomMessagesStream = ReceiverStream<Result<CustomMessage, Status>>;

    async fn subscribe_custom_messages(
        &self,
        request: Request<SubscribeCustomMessagesRequest>,
    ) -> Result<Response<Self::SubscribeCustomMessagesStream>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn list_aliases(
        &self,
        request: Request<ListAliasesRequest>,
    ) -> Result<Response<ListAliasesResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }

    async fn lookup_htlc_resolution(
        &self,
        request: Request<LookupHtlcResolutionRequest>,
    ) -> Result<Response<LookupHtlcResolutionResponse>, Status> {
        Err(Status::unimplemented("")) // todo
    }
}

fn invoice_to_lnrpc_invoice(invoice: crate::models::invoice::Invoice) -> Invoice {
    let bolt11 = invoice.bolt11();
    let state: InvoiceState = match invoice.status() {
        InvoiceStatus::Pending => InvoiceState::Open,
        InvoiceStatus::Expired => InvoiceState::Canceled,
        InvoiceStatus::Held => InvoiceState::Accepted,
        InvoiceStatus::Paid => InvoiceState::Settled,
    };
    let settle_date = if state == InvoiceState::Settled {
        invoice.updated_date()
    } else {
        0
    };

    let route_hints = bolt11
        .route_hints()
        .iter()
        .map(|hint| RouteHint {
            hop_hints: hint
                .0
                .iter()
                .map(|hop| HopHint {
                    node_id: hop.src_node_id.to_string(),
                    chan_id: hop.short_channel_id,
                    fee_base_msat: hop.fees.base_msat,
                    fee_proportional_millionths: hop.fees.proportional_millionths,
                    cltv_expiry_delta: hop.cltv_expiry_delta as u32,
                })
                .collect(),
        })
        .collect();

    let amt_paid_msat = if state == InvoiceState::Settled {
        bolt11.amount_milli_satoshis().unwrap_or(0) as i64
    } else {
        0
    };

    let (memo, description_hash) = match bolt11.description() {
        Bolt11InvoiceDescription::Direct(desc) => (desc.to_string(), vec![]),
        Bolt11InvoiceDescription::Hash(hash) => (String::new(), hash.0.to_byte_array().to_vec()),
    };

    let fallback_addr = bolt11.fallback_addresses().first().map(|a| a.to_string());

    Invoice {
        memo,
        r_preimage: invoice.preimage().map(|p| p.to_vec()).unwrap_or_default(),
        r_hash: bolt11.payment_hash().to_byte_array().to_vec(),
        value: bolt11
            .amount_milli_satoshis()
            .map(|a| a / 1_000)
            .unwrap_or(0) as i64,
        value_msat: bolt11.amount_milli_satoshis().unwrap_or(0) as i64,
        settled: state == InvoiceState::Settled,
        creation_date: invoice.creation_date(),
        settle_date,
        payment_request: bolt11.to_string(),
        description_hash,
        expiry: bolt11.expires_at().unwrap_or_default().as_secs() as i64,
        fallback_addr: fallback_addr.unwrap_or_default(),
        cltv_expiry: bolt11.min_final_cltv_expiry_delta(),
        route_hints,
        private: !bolt11.route_hints().is_empty(),
        add_index: invoice.id as u64,
        settle_index: invoice.id as u64,
        amt_paid: 0,
        amt_paid_sat: amt_paid_msat / 1_000,
        amt_paid_msat,
        state: state.into(),
        htlcs: vec![],
        features: Default::default(),
        is_keysend: false,
        payment_addr: bolt11.payment_secret().0.to_vec(),
        is_amp: false,
        amp_invoice_state: Default::default(),
    }
}

fn get_output_type(spk: &ScriptBuf) -> OutputScriptType {
    if spk.is_p2pkh() {
        OutputScriptType::ScriptTypePubkeyHash
    } else if spk.is_p2sh() {
        OutputScriptType::ScriptTypeScriptHash
    } else if spk.is_v0_p2wpkh() {
        OutputScriptType::ScriptTypeWitnessV0PubkeyHash
    } else if spk.is_v0_p2wsh() {
        OutputScriptType::ScriptTypeWitnessV0ScriptHash
    } else if spk.is_v1_p2tr() {
        OutputScriptType::ScriptTypeWitnessV1Taproot
    } else if spk.is_witness_program() {
        OutputScriptType::ScriptTypeWitnessUnknown
    } else if spk.is_p2pk() {
        OutputScriptType::ScriptTypePubkey
    } else if spk.is_op_return() {
        OutputScriptType::ScriptTypeNulldata
    } else {
        OutputScriptType::ScriptTypeNonStandard
    }
}
