#![allow(deprecated)]
#![allow(unused)]

use crate::channel_acceptor::{ChannelAcceptor, ChannelAcceptorRequest, ChannelAcceptorResponse};
use crate::events::PaymentAttempt;
use crate::invoicesrpc::invoices_server::Invoices;
use crate::invoicesrpc::*;
use crate::lndkrpc::offers_server::Offers;
use crate::lndkrpc::{
    Bolt12InvoiceContents, DecodeInvoiceRequest, GetInvoiceRequest, GetInvoiceResponse,
    PayInvoiceRequest, PayInvoiceResponse, PayOfferRequest, PayOfferResponse, PaymentPaths,
};
use crate::lnrpc::channel_point::FundingTxid;
use crate::lnrpc::failure::FailureCode;
use crate::lnrpc::fee_limit::Limit;
use crate::lnrpc::htlc_attempt::HtlcStatus;
use crate::lnrpc::invoice::InvoiceState;
use crate::lnrpc::lightning_server::Lightning;
use crate::lnrpc::payment::PaymentStatus;
use crate::lnrpc::pending_channels_response::{PendingChannel, PendingOpenChannel};
use crate::lnrpc::*;
use crate::models::channel_closure::ChannelClosure;
use crate::models::receive::{InvoiceStatus, Receive};
use crate::models::received_htlc::ReceivedHtlc;
use crate::models::routed_payment::RoutedPayment;
use crate::models::CreatedInvoice;
use crate::node::Node;
use crate::routerrpc::{
    self, BuildRouteRequest, BuildRouteResponse, ForwardHtlcInterceptRequest,
    ForwardHtlcInterceptResponse, GetMissionControlConfigRequest, GetMissionControlConfigResponse,
    HtlcEvent, PaymentState, QueryMissionControlRequest, QueryMissionControlResponse,
    QueryProbabilityRequest, QueryProbabilityResponse, ResetMissionControlRequest,
    ResetMissionControlResponse, RouteFeeRequest, RouteFeeResponse, SendPaymentRequest,
    SendToRouteResponse, SetMissionControlConfigRequest, SetMissionControlConfigResponse,
    SubscribeHtlcEventsRequest, TrackPaymentRequest, TrackPaymentsRequest, UpdateChanStatusRequest,
    UpdateChanStatusResponse, XImportMissionControlRequest, XImportMissionControlResponse,
};
use crate::signrpc::signer_server::Signer;
use crate::signrpc::{
    InputScriptResp, MuSig2CleanupRequest, MuSig2CleanupResponse, MuSig2CombineKeysRequest,
    MuSig2CombineKeysResponse, MuSig2CombineSigRequest, MuSig2CombineSigResponse,
    MuSig2RegisterNoncesRequest, MuSig2RegisterNoncesResponse, MuSig2SessionRequest,
    MuSig2SessionResponse, MuSig2SignRequest, MuSig2SignResponse, SharedKeyRequest,
    SharedKeyResponse, SignMessageReq, SignMessageResp, SignReq, SignResp, VerifyMessageReq,
    VerifyMessageResp,
};
use crate::walletrpc::wallet_kit_server::WalletKit;
use crate::walletrpc::{
    AddrRequest, AddrResponse, BumpFeeRequest, BumpFeeResponse, FinalizePsbtRequest,
    FinalizePsbtResponse, FundPsbtRequest, FundPsbtResponse, ImportAccountRequest,
    ImportAccountResponse, ImportPublicKeyRequest, ImportPublicKeyResponse, ImportTapscriptRequest,
    ImportTapscriptResponse, KeyReq, LabelTransactionRequest, LabelTransactionResponse,
    LeaseOutputRequest, LeaseOutputResponse, ListAccountsRequest, ListAccountsResponse,
    ListAddressesRequest, ListAddressesResponse, ListLeasesRequest, ListLeasesResponse,
    ListSweepsRequest, ListSweepsResponse, PendingSweepsRequest, PendingSweepsResponse,
    PublishResponse, ReleaseOutputRequest, ReleaseOutputResponse, RequiredReserveRequest,
    RequiredReserveResponse, SendOutputsRequest, SendOutputsResponse, SignMessageWithAddrRequest,
    SignMessageWithAddrResponse, SignPsbtRequest, SignPsbtResponse, VerifyMessageWithAddrRequest,
    VerifyMessageWithAddrResponse,
};
use crate::{lndkrpc, models};
use bdk_wallet::chain::ChainPosition;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::ecdsa::Signature;
use bitcoin::hashes::{sha256, sha256::Hash as Sha256, sha256d::Hash as Sha256d, Hash};
use bitcoin::hex::DisplayHex;
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use bitcoin::secp256k1::{Message, PublicKey, ThirtyTwoByteHash};
use bitcoin::{Address, FeeRate, Network, ScriptBuf, TxOut, Txid};
use bitcoincore_rpc::json::EstimateMode;
use bitcoincore_rpc::RpcApi;
use itertools::Itertools;
use lightning::blinded_path::payment::{BlindedPayInfo, BlindedPaymentPath};
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::events::bump_transaction::WalletSource;
use lightning::events::PathFailure;
use lightning::ln::bolt11_payment::{
    payment_parameters_from_invoice, payment_parameters_from_zero_amount_invoice,
};
use lightning::ln::channelmanager::{PaymentId, RecipientOnionFields, Retry};
use lightning::ln::types::ChannelId;
use lightning::ln::PaymentPreimage;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::Offer;
use lightning::routing::router::{PaymentParameters, RouteParameters, Router};
use lightning::sign::{EntropySource, NodeSigner, Recipient};
use lightning::util::config::MaxDustHTLCExposure;
use lightning::util::logger::Logger;
use lightning::util::ser::Writeable;
use lightning::{log_error, log_info, log_trace};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tokio::time::sleep;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::codegen::tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

#[tonic::async_trait]
impl Lightning for Node {
    async fn wallet_balance(
        &self,
        _request: Request<WalletBalanceRequest>,
    ) -> Result<Response<WalletBalanceResponse>, Status> {
        let balance = self.wallet.balance();

        let account = WalletAccountBalance {
            confirmed_balance: balance.trusted_spendable().to_sat() as i64,
            unconfirmed_balance: balance.untrusted_pending.to_sat() as i64,
        };

        let mut account_balance = HashMap::with_capacity(1);
        account_balance.insert("default".to_string(), account);

        let response = WalletBalanceResponse {
            total_balance: balance.total().to_sat() as i64,
            confirmed_balance: balance.trusted_spendable().to_sat() as i64,
            unconfirmed_balance: balance.untrusted_pending.to_sat() as i64,
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
            unsettled_local_balance: Some(Default::default()),
            unsettled_remote_balance: Some(Default::default()),
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
            return Err(Status::invalid_argument("account is not supported"));
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
                let (block_height, time_stamp) = match t.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => {
                        (anchor.block_id.height as i32, anchor.confirmation_time)
                    }
                    ChainPosition::Unconfirmed { last_seen } => (-1, last_seen.unwrap_or_default()),
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
                            amount: o.amount.to_sat() as i64,
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
                    amount: (t.received - t.sent).to_sat() as i64,
                    num_confirmations: current_height as i32 - block_height + 1,
                    block_hash,
                    block_height,
                    time_stamp: time_stamp as i64,
                    total_fees: t.fee.unwrap_or_default().to_sat() as i64,
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
                            script_pubkey: a.assume_checked().script_pubkey(),
                            value: bitcoin::Amount::from_sat(amt as u64),
                        })
                })
                .collect::<Result<Vec<TxOut>, Status>>()?;

            let fee_sat = self
                .wallet
                .estimate_fee_to_outputs(outputs, Some(fee_rate))
                .map_err(|e| Status::internal(e.to_string()))?;
            let response = EstimateFeeResponse {
                fee_sat: fee_sat.to_sat() as i64,
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
            return Err(Status::invalid_argument("sat_per_byte is not supported"));
        }

        if req.sat_per_vbyte != 0 && req.target_conf != 0 {
            return Err(Status::invalid_argument(
                "Cannot have both sat_per_vbyte and target_conf",
            ));
        }

        if req.amount == 0 && !req.send_all {
            return Err(Status::invalid_argument("amount or send_all is required"));
        }

        if req.min_confs != 0 && req.min_confs != 1 {
            return Err(Status::invalid_argument("min_confs is not supported"));
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
                .send_to_address(
                    address,
                    bitcoin::Amount::from_sat(req.amount as u64),
                    fee_rate,
                )
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
            return Err(Status::invalid_argument("account is not supported"));
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
                let confs = match u.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => {
                        current_height as i32 - anchor.block_id.height as i32 + 1
                    }
                    ChainPosition::Unconfirmed { .. } => 0,
                };

                if confs >= min_confs && confs <= max_confs {
                    let address_type = if u.txout.script_pubkey.is_p2tr() {
                        AddressType::TaprootPubkey
                    } else if u.txout.script_pubkey.is_p2wpkh() {
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
                        amount_sat: u.txout.value.to_sat() as i64,
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
        Err(Status::unimplemented("subscribe_transactions")) // todo
    }

    async fn send_many(
        &self,
        request: Request<SendManyRequest>,
    ) -> Result<Response<SendManyResponse>, Status> {
        let req = request.into_inner();

        if req.sat_per_byte != 0 {
            return Err(Status::invalid_argument("sat_per_byte is not supported"));
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
            return Err(Status::invalid_argument("min_confs is not supported"));
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
                        script_pubkey: a.assume_checked().script_pubkey(),
                        value: bitcoin::Amount::from_sat(amt as u64),
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
        );
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
        let peers = self.peer_manager.list_peers();

        let peers = peers
            .into_iter()
            .map(|peer| Peer {
                pub_key: peer.counterparty_node_id.to_string(),
                address: peer
                    .socket_address
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
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
        Err(Status::unimplemented("subscribe_peer_events")) // todo
    }

    async fn get_info(
        &self,
        _: Request<GetInfoRequest>,
    ) -> Result<Response<GetInfoResponse>, Status> {
        let num_peers = self.peer_manager.list_peers().len() as u32;
        let channels = self.channel_manager.list_channels();
        let best_block = self.channel_manager.current_best_block();
        let wallet_block = self.wallet.sync_height().unwrap_or_default();

        let chain = Chain {
            chain: "bitcoin".to_string(),
            network: self.network.to_string(),
        };

        let resp = GetInfoResponse {
            version: "0.17.5-beta".to_string(),
            commit_hash: "unknown".to_string(),
            identity_pubkey: self.node_id().to_string(),
            alias: self.config.alias().to_string(),
            color: "#000000".to_string(),
            num_pending_channels: channels.iter().filter(|c| !c.is_channel_ready).count() as u32,
            num_active_channels: channels.iter().filter(|c| c.is_usable).count() as u32,
            num_inactive_channels: channels
                .iter()
                .filter(|c| c.is_channel_ready && !c.is_usable)
                .count() as u32,
            num_peers,
            block_height: best_block.height,
            block_hash: best_block.block_hash.to_string(),
            best_header_timestamp: 0,
            synced_to_chain: wallet_block.height >= best_block.height,
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
            .map(|c| {
                let commitment_type = if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_taproot())
                {
                    CommitmentType::SimpleTaproot
                } else if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_anchors_zero_fee_htlc_tx())
                {
                    CommitmentType::Anchors
                } else if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_static_remote_key())
                {
                    CommitmentType::StaticRemoteKey
                } else {
                    CommitmentType::Legacy
                };
                PendingOpenChannel {
                    channel: Some(PendingChannel {
                        remote_node_pub: c.counterparty.node_id.to_string(),
                        channel_point: c.funding_txo.map(|t| t.to_string()).unwrap_or_default(),
                        capacity: c.channel_value_satoshis as i64,
                        local_balance: (c.balance_msat / 1_000) as i64,
                        remote_balance: (c.channel_value_satoshis - (c.balance_msat / 1_000))
                            as i64,
                        local_chan_reserve_sat: c.unspendable_punishment_reserve.unwrap_or_default()
                            as i64,
                        remote_chan_reserve_sat: c.counterparty.unspendable_punishment_reserve
                            as i64,
                        initiator: if c.is_outbound { 1 } else { 2 },
                        commitment_type: commitment_type.into(),
                        num_forwarding_packages: 0,
                        chan_status_flags: "".to_string(),
                        private: !c.is_announced,
                        memo: "".to_string(),
                    }),
                    commit_fee: 0,
                    commit_weight: 0,
                    fee_per_kw: 0,
                    funding_expiry_blocks: 0,
                }
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
            .map(|c| {
                let commitment_type = if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_taproot())
                {
                    CommitmentType::SimpleTaproot
                } else if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_anchors_zero_fee_htlc_tx())
                {
                    CommitmentType::Anchors
                } else if c
                    .channel_type
                    .as_ref()
                    .is_some_and(|t| t.supports_static_remote_key())
                {
                    CommitmentType::StaticRemoteKey
                } else {
                    CommitmentType::Legacy
                };
                Channel {
                    active: c.is_usable,
                    remote_pubkey: c.counterparty.node_id.to_string(),
                    channel_point: c.funding_txo.map(|t| t.to_string()).unwrap_or_default(),
                    chan_id: c.short_channel_id.unwrap_or_default(),
                    capacity: c.channel_value_satoshis as i64,
                    local_balance: c.balance_msat as i64 / 1_000,
                    remote_balance: (c.channel_value_satoshis - (c.balance_msat / 1_000)) as i64,
                    commit_fee: 0,
                    commit_weight: 0,
                    fee_per_kw: c.feerate_sat_per_1000_weight.unwrap_or_default() as i64,
                    unsettled_balance: 0,
                    total_satoshis_sent: 0,
                    total_satoshis_received: 0,
                    num_updates: 0,
                    pending_htlcs: vec![],
                    csv_delay: c.force_close_spend_delay.unwrap_or_default() as u32,
                    private: !c.is_announced,
                    initiator: c.is_outbound,
                    chan_status_flags: "".to_string(),
                    local_chan_reserve_sat: c.unspendable_punishment_reserve.unwrap_or_default()
                        as i64,
                    remote_chan_reserve_sat: c.counterparty.unspendable_punishment_reserve as i64,
                    static_remote_key: true,
                    commitment_type: commitment_type.into(),
                    lifetime: 0,
                    uptime: 0,
                    close_address: "".to_string(),
                    push_amount_sat: 0,
                    thaw_height: 0,
                    local_constraints: Some(ChannelConstraints {
                        csv_delay: c.force_close_spend_delay.unwrap_or_default() as u32,
                        chan_reserve_sat: c.unspendable_punishment_reserve.unwrap_or_default(),
                        dust_limit_sat: match c.config.unwrap().max_dust_htlc_exposure {
                            MaxDustHTLCExposure::FixedLimitMsat(msats) => msats / 1000,
                            MaxDustHTLCExposure::FeeRateMultiplier(_) => 0, // todo
                        },
                        max_pending_amt_msat: c.outbound_capacity_msat,
                        min_htlc_msat: c.next_outbound_htlc_minimum_msat,
                        max_accepted_htlcs: 40, // todo
                    }),
                    remote_constraints: Some(ChannelConstraints {
                        csv_delay: c.force_close_spend_delay.unwrap_or_default() as u32,
                        chan_reserve_sat: c.counterparty.unspendable_punishment_reserve,
                        dust_limit_sat: 0, // todo
                        max_pending_amt_msat: c.inbound_capacity_msat,
                        min_htlc_msat: c.inbound_htlc_minimum_msat.unwrap_or_default(),
                        max_accepted_htlcs: 40, // todo
                    }),
                    alias_scids: c.inbound_scid_alias.map(|a| vec![a]).unwrap_or_default(),
                    zero_conf: false,
                    zero_conf_confirmed_scid: 0,
                    peer_alias: "".to_string(),
                    peer_scid_alias: c.outbound_scid_alias.unwrap_or_default(),
                    memo: "".to_string(),
                }
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
        Err(Status::unimplemented("subscribe_channel_events")) // todo
    }

    async fn closed_channels(
        &self,
        request: Request<ClosedChannelsRequest>,
    ) -> Result<Response<ClosedChannelsResponse>, Status> {
        let req = request.into_inner();

        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(format!("Error getting database connection: {e}")))?;

        let closures = ChannelClosure::list_closures(&mut conn)
            .map_err(|e| Status::internal(format!("Error listing channel closures: {e}")))?;

        let chain_hash = self.network.chain_hash().to_string();
        let channels: Vec<ChannelCloseSummary> = closures
            .into_iter()
            .map(|c| {
                // todo missing a bunch of fields
                ChannelCloseSummary {
                    channel_point: c.funding_txo().map(|x| x.to_string()).unwrap_or_default(),
                    chan_id: c.id as u64,
                    chain_hash: chain_hash.clone(),
                    closing_tx_hash: "".to_string(),
                    remote_pubkey: c.node_id().to_string(),
                    capacity: 0,
                    close_height: 0,
                    settled_balance: 0,
                    time_locked_balance: 0,
                    close_type: 0,
                    open_initiator: 0,
                    close_initiator: 0,
                    resolutions: vec![],
                    alias_scids: vec![],
                    zero_conf_confirmed_scid: 0,
                }
            })
            .collect();

        let resp = ClosedChannelsResponse { channels };
        Ok(Response::new(resp))
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

        // todo handle a bunch of other flags
        let outpoint = self
            .open_channel_with_timeout(
                pk,
                req.local_funding_amount as u64,
                req.push_sat as u64 * 1_000,
                fee_rate,
                req.private,
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
        let req = request.into_inner();

        let pk = if req.node_pubkey_string.is_empty() {
            PublicKey::from_slice(&req.node_pubkey)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?
        } else {
            PublicKey::from_str(&req.node_pubkey_string)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<OpenStatusUpdate, Status>>(128);

        let fee_rate = if req.sat_per_vbyte > 0 {
            Some(req.sat_per_vbyte as i32)
        } else {
            None
        };

        // todo handle a bunch of other flags
        let (channel_id, id) = self
            .init_open_channel(
                pk,
                req.local_funding_amount as u64,
                req.push_sat as u64 * 1_000,
                fee_rate,
                req.private,
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let outpoint = self
            .await_chan_funding_tx(id, &pk, 30)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let pending_update = PendingUpdate {
            txid: outpoint.txid.encode(),
            output_index: outpoint.vout,
        };
        let update = OpenStatusUpdate {
            pending_chan_id: channel_id.0.to_vec(),
            update: Some(open_status_update::Update::ChanPending(pending_update)),
        };

        tx.send(Ok(update))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let cm = self.channel_manager.clone();
        tokio::spawn(async move {
            // wait for confirmations
            // todo could use the event manager for this
            loop {
                if tx.is_closed() {
                    break;
                }

                if let Some(chan) = cm
                    .list_channels_with_counterparty(&pk)
                    .iter()
                    .find(|c| c.funding_txo.map(|f| f.into_bitcoin_outpoint()) == Some(outpoint))
                {
                    if chan.confirmations_required.unwrap_or(0) <= chan.confirmations.unwrap_or(0) {
                        break;
                    }
                }
                sleep(Duration::from_secs(5)).await;
            }

            let resp = ChannelPoint {
                output_index: outpoint.vout,
                funding_txid: Some(FundingTxid::FundingTxidStr(outpoint.txid.to_string())),
            };
            let open_update = ChannelOpenUpdate {
                channel_point: Some(resp),
            };
            let update = OpenStatusUpdate {
                pending_chan_id: channel_id.0.to_vec(),
                update: Some(open_status_update::Update::ChanOpen(open_update)),
            };

            if tx.is_closed() {
                return;
            }

            tx.send(Ok(update)).await.unwrap();
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn batch_open_channel(
        &self,
        request: Request<BatchOpenChannelRequest>,
    ) -> Result<Response<BatchOpenChannelResponse>, Status> {
        Err(Status::unimplemented("batch_open_channel")) // todo
    }

    async fn funding_state_step(
        &self,
        request: Request<FundingTransitionMsg>,
    ) -> Result<Response<FundingStateStepResp>, Status> {
        Err(Status::unimplemented("funding_state_step")) // todo
    }

    type ChannelAcceptorStream = ReceiverStream<Result<ChannelAcceptRequest, Status>>;

    async fn channel_acceptor(
        &self,
        request: Request<Streaming<ChannelAcceptResponse>>,
    ) -> Result<Response<Self::ChannelAcceptorStream>, Status> {
        let mut req = request.into_inner();

        let mut lock = self.channel_acceptor.write().await;

        // check if channel_acceptor is already registered
        if lock.is_some() {
            return Err(Status::invalid_argument(
                "channel_acceptor already registered",
            ));
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ChannelAcceptRequest, Status>>(128);

        let (sender, mut resps) = tokio::sync::broadcast::channel::<ChannelAcceptorRequest>(100);
        *lock = Some(ChannelAcceptor::new(sender));
        drop(lock);

        tokio::spawn(async move {
            while let Ok(item) = resps.recv().await {
                let i = ChannelAcceptRequest {
                    node_pubkey: item.node_pubkey.encode(),
                    chain_hash: item.chain_hash,
                    pending_chan_id: item.pending_chan_id.encode(),
                    funding_amt: item.funding_amt,
                    push_amt: item.push_amt,
                    dust_limit: item.dust_limit,
                    max_value_in_flight: item.max_value_in_flight,
                    channel_reserve: item.channel_reserve,
                    min_htlc: item.min_htlc,
                    fee_per_kw: item.fee_per_kw,
                    csv_delay: item.csv_delay,
                    max_accepted_htlcs: item.max_accepted_htlcs,
                    channel_flags: item.channel_flags,
                    commitment_type: item.commitment_type,
                    wants_zero_conf: item.wants_zero_conf,
                    wants_scid_alias: item.wants_scid_alias,
                };
                tx.send(Ok(i)).await.unwrap();
            }
        });

        let lock = self.channel_acceptor.clone();
        let network = self.network;
        tokio::spawn(async move {
            while let Ok(item) = req.message().await {
                if let Some(item) = item {
                    if let Some(channel_acceptor) = lock.read().await.as_ref() {
                        let upfront_shutdown = if item.upfront_shutdown.is_empty() {
                            None
                        } else {
                            // todo handle errors
                            Address::from_str(&item.upfront_shutdown)
                                .ok()
                                .and_then(|a| a.require_network(network).ok())
                        };

                        // todo handle unwraps
                        let resp = ChannelAcceptorResponse {
                            accept: item.accept,
                            pending_chan_id: ChannelId(item.pending_chan_id.try_into().unwrap()),
                            error: item.error,
                            upfront_shutdown,
                            csv_delay: item.csv_delay,
                            reserve_sat: item.reserve_sat,
                            in_flight_max_msat: item.in_flight_max_msat,
                            max_htlc_count: item.max_htlc_count,
                            min_htlc_in: item.min_htlc_in,
                            min_accept_depth: item.min_accept_depth,
                            zero_conf: item.zero_conf,
                        };
                        let _ = channel_acceptor.send_response(resp).await;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type CloseChannelStream = ReceiverStream<Result<CloseStatusUpdate, Status>>;

    async fn close_channel(
        &self,
        request: Request<CloseChannelRequest>,
    ) -> Result<Response<Self::CloseChannelStream>, Status> {
        let req = request.into_inner();

        let channel_point = match req.channel_point {
            Some(point) => match point.funding_txid {
                Some(FundingTxid::FundingTxidStr(txid)) => {
                    let txid = Txid::from_str(&txid)
                        .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;
                    lightning::chain::transaction::OutPoint {
                        txid,
                        index: point.output_index as u16,
                    }
                }
                Some(FundingTxid::FundingTxidBytes(txid)) => {
                    let txid = Txid::from_slice(&txid)
                        .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;
                    lightning::chain::transaction::OutPoint {
                        txid,
                        index: point.output_index as u16,
                    }
                }
                None => return Err(Status::invalid_argument("funding_txid is required")),
            },
            None => return Err(Status::invalid_argument("channel_point is required")),
        };

        let chan = self
            .channel_manager
            .list_channels()
            .into_iter()
            .find(|c| c.funding_txo == Some(channel_point))
            .ok_or(Status::invalid_argument("Channel not found"))?;

        let res = if req.force {
            self.channel_manager.force_close_broadcasting_latest_txn(
                &chan.channel_id,
                &chan.counterparty.node_id,
                "User forced closed channel.".to_string(),
            )
        } else if req.delivery_address.is_empty() {
            self.channel_manager
                .close_channel(&chan.channel_id, &chan.counterparty.node_id)
        } else {
            let address = Address::from_str(&req.delivery_address)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?
                .require_network(self.network)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;
            let script = address
                .script_pubkey()
                .try_into()
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;
            self.channel_manager.close_channel_with_feerate_and_script(
                &chan.channel_id,
                &chan.counterparty.node_id,
                None,
                Some(script),
            )
        };
        res.map_err(|e| Status::invalid_argument(format!("API error: {e:?}")))?;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<CloseStatusUpdate, Status>>(5);

        let bitcoind = self.bitcoind.clone();
        let chain_monitor = self.chain_monitor.clone();
        tokio::spawn(async move {
            let mut closing_txid: Option<Txid> = None;
            loop {
                if tx.is_closed() {
                    return;
                }

                let mut resp: Option<CloseStatusUpdate> = None;
                if let Ok(chan) = chain_monitor.get_monitor(channel_point) {
                    if let Some((txid, _, _)) = chan.get_relevant_txids().first() {
                        let update = close_status_update::Update::ClosePending(PendingUpdate {
                            txid: txid.encode(),
                            output_index: 0, // todo this is wrong
                        });
                        resp = Some(CloseStatusUpdate {
                            update: Some(update),
                        });
                        closing_txid = Some(*txid);
                    }
                };

                if let Some(resp) = resp {
                    tx.send(Ok(resp)).await.unwrap();
                    break;
                }

                sleep(Duration::from_secs(5)).await;
            }

            if closing_txid.is_none() {
                return;
            }

            loop {
                if tx.is_closed() {
                    return;
                }

                if let Ok(info) = bitcoind.get_raw_transaction_info(&closing_txid.unwrap(), None) {
                    if info.confirmations.unwrap_or(0) >= 6 {
                        let update = close_status_update::Update::ChanClose(ChannelCloseUpdate {
                            closing_txid: closing_txid.encode(),
                            success: true,
                        });
                        let resp = CloseStatusUpdate {
                            update: Some(update),
                        };
                        tx.send(Ok(resp)).await.unwrap();
                        return;
                    }
                }

                sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn abandon_channel(
        &self,
        request: Request<AbandonChannelRequest>,
    ) -> Result<Response<AbandonChannelResponse>, Status> {
        Err(Status::unimplemented("abandon_channel")) // todo
    }

    type SendPaymentStream = ReceiverStream<Result<SendResponse, Status>>;

    async fn send_payment(
        &self,
        request: Request<Streaming<SendRequest>>,
    ) -> Result<Response<Self::SendPaymentStream>, Status> {
        log_trace!(self.logger, "send_payment");
        let mut stream = request.into_inner();
        let req = stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("Error receiving payment request: {e}")))?
            .ok_or_else(|| Status::invalid_argument("No payment request provided"))?;

        let amount_msats = if req.amt > 0 && req.amt_msat > 0 {
            return Err(Status::invalid_argument("Cannot have amt and amt_msat"));
        } else if req.amt_msat > 0 {
            Some(req.amt_msat as u64)
        } else if req.amt > 0 {
            Some(req.amt as u64 * 1_000)
        } else {
            None
        };

        let mut value_msats: i64;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<SendResponse, Status>>(128);

        let invoice: Option<Bolt11Invoice> = if req.payment_request.is_empty() {
            None
        } else {
            Some(
                Bolt11Invoice::from_str(&req.payment_request)
                    .map_err(|e| Status::invalid_argument(format!("Invalid invoice: {e:?}")))?,
            )
        };

        let (hash, onion, route_params, pk) = if invoice.is_none() {
            if amount_msats.is_none() {
                return Err(Status::invalid_argument("amt or amt_msat is required"));
            }
            let amount_msats = amount_msats.unwrap();

            let pk = PublicKey::from_slice(&req.dest)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;

            let tlvs = req.dest_custom_records.into_iter().collect_vec();
            let onion = RecipientOnionFields::spontaneous_empty()
                .with_custom_tlvs(tlvs)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;

            let preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
            let payment_hash = sha256::Hash::hash(&preimage.0);

            let max_total_routing_fee_msat: Option<u64> = match req.fee_limit.and_then(|l| l.limit)
            {
                None => None,
                Some(Limit::Fixed(sats)) => Some(sats as u64 * 1_000),
                Some(Limit::FixedMsat(msats)) => Some(msats as u64),
                Some(Limit::Percent(percent)) => Some((percent as u64 * amount_msats) / 100),
            };

            let payment_params = PaymentParameters::for_keysend(pk, 40, false);
            let route_params: RouteParameters = RouteParameters {
                final_value_msat: amount_msats,
                payment_params,
                max_total_routing_fee_msat,
            };

            (
                lightning::ln::PaymentHash(payment_hash.to_byte_array()),
                onion,
                route_params,
                pk,
            )
        } else if let Some(invoice) = invoice.as_ref() {
            if invoice
                .amount_milli_satoshis()
                .is_some_and(|a| a != amount_msats.unwrap_or(a))
            {
                return Err(Status::invalid_argument(
                    "Invoice amount does not match request",
                ));
            }

            if invoice.network() != self.network {
                return Err(Status::invalid_argument("Invoice is on the wrong network"));
            }

            let pk = invoice.get_payee_pub_key();
            let (hash, onion, route_params) = match invoice.amount_milli_satoshis() {
                Some(_) => payment_parameters_from_invoice(invoice).expect("already checked"),
                None => match amount_msats {
                    Some(msats) => payment_parameters_from_zero_amount_invoice(invoice, msats)
                        .expect("already checked"),
                    None => {
                        return Err(Status::invalid_argument("Amount missing from request"));
                    }
                },
            };

            (hash, onion, route_params, pk)
        } else {
            return Err(Status::invalid_argument("Invalid payment params"));
        };

        let amount_msats = route_params.final_value_msat;

        let payment_id = PaymentId(hash.0);

        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(format!("Error getting database connection: {e}")))?;
        models::payment::Payment::create(
            &mut conn,
            payment_id,
            hash,
            amount_msats as i64,
            Some(pk),
            invoice,
            None,
        )
        .map_err(|e| Status::internal(format!("Error creating payment db entry: {e}")))?;

        value_msats = amount_msats as i64;

        let mut pay_rx = self.payment_attempt_broadcast.subscribe();
        let payment_request = req.payment_request.clone();
        let self_clone = self.clone();
        tokio::spawn(async move {
            let mut attempts = 0;

            let start = SystemTime::now();

            let node_id = self_clone.node_id();
            loop {
                if tx.is_closed() {
                    break;
                }

                if SystemTime::now().duration_since(start).unwrap().as_secs() > 60 {
                    let resp = SendResponse {
                        payment_error: "Timeout".to_string(),
                        payment_preimage: vec![],
                        payment_route: None,
                        payment_hash: hash.encode(),
                    };

                    let _ = tx.send(Ok(resp)).await;
                    return;
                }

                let route = self_clone
                    .router
                    .find_route(
                        &node_id,
                        &route_params,
                        None,
                        self_clone.channel_manager.compute_inflight_htlcs(),
                    )
                    .unwrap();

                let hops = route
                    .paths
                    .iter()
                    .flat_map(|path| {
                        let mut amt_to_forward_msat = path.final_value_msat() + path.fee_msat();

                        path.hops.iter().map(move |hop| {
                            amt_to_forward_msat -= hop.fee_msat;
                            let fee_msat = if hop.fee_msat == path.final_value_msat() {
                                0_i64
                            } else {
                                hop.fee_msat as i64
                            };
                            Hop {
                                chan_id: hop.short_channel_id,
                                chan_capacity: amt_to_forward_msat as i64,
                                amt_to_forward: amt_to_forward_msat as i64,
                                fee: fee_msat / 1_000,
                                expiry: hop.cltv_expiry_delta,
                                amt_to_forward_msat: amt_to_forward_msat as i64,
                                fee_msat,
                                pub_key: hop.pubkey.to_string(),
                                tlv_payload: true,
                                mpp_record: None,
                                amp_record: None,
                                custom_records: Default::default(),
                                metadata: vec![],
                            }
                        })
                    })
                    .collect();

                let payment_route = Route {
                    total_time_lock: route
                        .paths
                        .iter()
                        .map(|p| p.final_cltv_expiry_delta().unwrap_or_default())
                        .max()
                        .unwrap_or_default(),
                    total_fees: (route.get_total_fees() / 1_000) as i64,
                    total_amt: value_msats / 1_000,
                    hops,
                    total_fees_msat: route.get_total_fees() as i64,
                    total_amt_msat: value_msats,
                };

                match self_clone.channel_manager.send_payment_with_route(
                    route,
                    hash,
                    onion.clone(),
                    payment_id,
                ) {
                    Ok(_) => {
                        let resp = SendResponse {
                            payment_error: "".to_string(),
                            payment_preimage: vec![],
                            payment_route: Some(payment_route.clone()),
                            payment_hash: hash.encode(),
                        };

                        let _ = tx.send(Ok(resp)).await;
                    }
                    Err(e) => {
                        tx.send(Err(Status::internal(format!(
                            "Error sending payment: {e:?}"
                        ))));
                        break;
                    }
                }

                loop {
                    if let Ok(attempt) = pay_rx.recv().await {
                        if attempt.payment_id() != payment_id {
                            continue;
                        }

                        match attempt {
                            PaymentAttempt::Successful { path, .. } => {
                                let res = self_clone.await_payment(payment_id, 60).await.unwrap();
                                let resp = SendResponse {
                                    payment_error: "".to_string(),
                                    payment_preimage: res
                                        .preimage()
                                        .map(|x| x.encode())
                                        .unwrap_or_default(),
                                    payment_route: Some(payment_route),
                                    payment_hash: hash.encode(),
                                };

                                let _ = tx.send(Ok(resp)).await;

                                return;
                            }
                            PaymentAttempt::Failed {
                                failure,
                                path,
                                payment_failed_permanently,
                                ..
                            } => {
                                if payment_failed_permanently {
                                    let resp = SendResponse {
                                        payment_error: "Payment failed".to_string(),
                                        payment_preimage: vec![],
                                        payment_route: Some(payment_route),
                                        payment_hash: hash.encode(),
                                    };

                                    let _ = tx.send(Ok(resp)).await;
                                }
                            }
                        }
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn send_payment_sync(
        &self,
        request: Request<SendRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        let req = request.into_inner();

        let amount_msats = if req.amt > 0 && req.amt_msat > 0 {
            return Err(Status::invalid_argument("Cannot have amt and amt_msat"));
        } else if req.amt_msat > 0 {
            Some(req.amt_msat as u64)
        } else if req.amt > 0 {
            Some(req.amt as u64 * 1_000)
        } else {
            None
        };

        let result = if req.payment_request.is_empty() {
            if amount_msats.is_none() {
                return Err(Status::invalid_argument("amt or amt_msat is required"));
            }

            let node_id = PublicKey::from_slice(&req.dest)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;

            let tlvs = req.dest_custom_records.into_iter().collect_vec();

            self.keysend_with_timeout(node_id, amount_msats.unwrap(), tlvs, Some(5 * 60)) // default 5 minute timeout
                .await
        } else if let Ok(invoice) = Bolt11Invoice::from_str(&req.payment_request) {
            if invoice
                .amount_milli_satoshis()
                .is_some_and(|a| a != amount_msats.unwrap_or(a))
            {
                return Err(Status::invalid_argument(
                    "Invoice amount does not match request",
                ));
            }

            let payment_hash = invoice.payment_hash().as_byte_array().to_vec();
            self.pay_invoice_with_timeout(invoice, amount_msats, Some(5 * 60)) // default 5 minute timeout
                .await
        } else {
            return Err(Status::invalid_argument("Invalid payment params"));
        };

        let res = match result {
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
                    total_fees: payment.fee_msats.unwrap_or_default() / 1_000,
                    total_amt: payment.amount_msats / 1_000,
                    hops,
                    total_fees_msat: payment.amount_msats,
                    total_amt_msat: payment.amount_msats,
                };

                SendResponse {
                    payment_error: "".to_string(),
                    payment_preimage: payment.preimage().map(|p| p.to_vec()).unwrap_or_default(),
                    payment_route: None,
                    payment_hash: payment.payment_hash().to_vec(),
                }
            }
            Err(e) => SendResponse {
                payment_error: e.to_string(),
                payment_preimage: vec![],
                payment_route: None,
                payment_hash: vec![],
            },
        };

        Ok(Response::new(res))
    }

    type SendToRouteStream = ReceiverStream<Result<SendResponse, Status>>;

    async fn send_to_route(
        &self,
        request: Request<Streaming<SendToRouteRequest>>,
    ) -> Result<Response<Self::SendToRouteStream>, Status> {
        Err(Status::unimplemented("send_to_route")) // todo
    }

    async fn send_to_route_sync(
        &self,
        request: Request<SendToRouteRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        Err(Status::unimplemented("send_to_route_sync")) // todo
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
        _: Request<ListInvoiceRequest>, // todo pagination
    ) -> Result<Response<ListInvoiceResponse>, Status> {
        let invoices = self
            .list_receives()
            .map_err(|e| Status::internal(e.to_string()))?;

        // todo htlcs
        let invoices = invoices.into_iter().map(receive_to_lnrpc_invoice).collect();

        let resp = ListInvoiceResponse {
            invoices,
            last_index_offset: 0,
            first_index_offset: 0,
        };

        Ok(Response::new(resp))
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
        let opt = Receive::find_by_payment_hash(&mut conn, &req.r_hash)
            .map_err(|e| Status::internal(e.to_string()))?;

        match opt {
            Some(invoice) => {
                let htlcs = ReceivedHtlc::find_by_receive_id(&mut conn, invoice.id)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .into_iter()
                    .map(|h| InvoiceHtlc {
                        chan_id: 0, // todo
                        htlc_index: h.id as u64,
                        amt_msat: h.amount_msats as u64,
                        accept_height: 0,
                        accept_time: 0,
                        resolve_time: 0,
                        expiry_height: h.cltv_expiry as i32,
                        state: InvoiceHtlcState::Settled as i32,
                        custom_records: Default::default(),
                        mpp_total_amt_msat: invoice.amount_msats.unwrap_or_default() as u64,
                        amp: None,
                    })
                    .collect_vec();
                let mut invoice = receive_to_lnrpc_invoice(invoice);
                invoice.htlcs = htlcs;
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

                // todo htlcs
                let invoice = receive_to_lnrpc_invoice(item);
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
            destination: invoice.get_payee_pub_key().to_string(),
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
        _: Request<ListPaymentsRequest>, // todo use request
    ) -> Result<Response<ListPaymentsResponse>, Status> {
        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(e.to_string()))?;

        let payments = crate::models::payment::Payment::list_payments(&mut conn)
            .map_err(|e| Status::internal(e.to_string()))?;

        let payments = payments
            .into_iter()
            .map(|p| {
                let status = p.status();
                Payment {
                    payment_preimage: p.preimage().map(hex::encode).unwrap_or_default(),
                    payment_request: p.bolt11().map(|b| b.to_string()).unwrap_or_default(),
                    payment_hash: hex::encode(p.payment_hash),
                    value: 0,
                    creation_date: p.created_at.and_utc().timestamp(),
                    fee: 0,
                    value_sat: (p.amount_msats / 1000),
                    value_msat: p.amount_msats,
                    payment_index: p.id as u64,
                    fee_sat: p.fee_msats.unwrap_or_default() / 1000,
                    fee_msat: p.fee_msats.unwrap_or_default(),
                    creation_time_ns: p.created_at.timestamp_nanos(),
                    status: status as i32,
                    htlcs: vec![],
                    failure_reason: 0, // todo
                }
            })
            .collect::<Vec<_>>();

        Ok(Response::new(ListPaymentsResponse {
            total_num_payments: payments.len() as u64,
            payments,
            first_index_offset: 0,
            last_index_offset: 0,
        }))
    }

    async fn delete_payment(
        &self,
        request: Request<DeletePaymentRequest>,
    ) -> Result<Response<DeletePaymentResponse>, Status> {
        Err(Status::unimplemented("delete_payment")) // todo
    }

    async fn delete_all_payments(
        &self,
        request: Request<DeleteAllPaymentsRequest>,
    ) -> Result<Response<DeleteAllPaymentsResponse>, Status> {
        Err(Status::unimplemented("delete_all_payments")) // todo
    }

    async fn describe_graph(
        &self,
        request: Request<ChannelGraphRequest>,
    ) -> Result<Response<ChannelGraph>, Status> {
        Err(Status::unimplemented("describe_graph")) // todo
    }

    async fn get_node_metrics(
        &self,
        request: Request<NodeMetricsRequest>,
    ) -> Result<Response<NodeMetricsResponse>, Status> {
        Err(Status::unimplemented("get_node_metrics")) // todo
    }

    async fn get_chan_info(
        &self,
        request: Request<ChanInfoRequest>,
    ) -> Result<Response<ChannelEdge>, Status> {
        Err(Status::unimplemented("get_chan_info")) // todo
    }

    async fn get_node_info(
        &self,
        request: Request<NodeInfoRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        Err(Status::unimplemented("get_node_info")) // todo
    }

    async fn query_routes(
        &self,
        request: Request<QueryRoutesRequest>,
    ) -> Result<Response<QueryRoutesResponse>, Status> {
        let req = request.into_inner();
        let pk = PublicKey::from_str(&req.pub_key)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let inflight = self.channel_manager.compute_inflight_htlcs();

        if !req.source_pub_key.is_empty() {
            return Err(Status::invalid_argument("source_pub_key is not supported"));
        }

        let final_value_msat = if req.amt_msat > 0 {
            req.amt_msat as u64
        } else if req.amt > 0 {
            req.amt as u64 * 1_000
        } else {
            return Err(Status::invalid_argument("amt or amt_msat is required"));
        };

        let max_total_routing_fee_msat = req.fee_limit.and_then(|l| l.limit).map(|l| match l {
            Limit::Fixed(sats) => (sats * 1_000) as u64,
            Limit::FixedMsat(msats) => msats as u64,
            Limit::Percent(percent) => (percent as u64 * final_value_msat / 100),
        });

        let mut payment_params = PaymentParameters::from_node_id(pk, req.final_cltv_delta as u32);
        let route_params = RouteParameters {
            payment_params,
            final_value_msat,
            max_total_routing_fee_msat,
        };

        match self.router.find_route(&pk, &route_params, None, inflight) {
            Ok(route) => {
                let routes = route
                    .paths
                    .into_iter()
                    .map(|p| {
                        let hops: Vec<Hop> = p
                            .hops
                            .iter()
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

                        Route {
                            total_time_lock: p.final_cltv_expiry_delta().unwrap_or_default(),
                            total_fees: (p.fee_msat() / 1_000) as i64,
                            total_amt: (p.final_value_msat() / 1_000) as i64,
                            hops,
                            total_fees_msat: p.fee_msat() as i64,
                            total_amt_msat: p.final_value_msat() as i64,
                        }
                    })
                    .collect();

                let resp = QueryRoutesResponse {
                    routes,
                    success_prob: 50.0, // todo
                };

                Ok(Response::new(resp))
            }
            Err(e) => {
                return Err(Status::internal(format!("{e:?}")));
            }
        }
    }

    async fn get_network_info(
        &self,
        request: Request<NetworkInfoRequest>,
    ) -> Result<Response<NetworkInfo>, Status> {
        Err(Status::unimplemented("get_network_info")) // todo
    }

    async fn stop_daemon(&self, _: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        self.stop_listen_connect.store(true, Ordering::Relaxed);

        Ok(Response::new(StopResponse {}))
    }

    type SubscribeChannelGraphStream = ReceiverStream<Result<GraphTopologyUpdate, Status>>;

    async fn subscribe_channel_graph(
        &self,
        request: Request<GraphTopologySubscription>,
    ) -> Result<Response<Self::SubscribeChannelGraphStream>, Status> {
        Err(Status::unimplemented("subscribe_channel_graph")) // todo
    }

    async fn debug_level(
        &self,
        request: Request<DebugLevelRequest>,
    ) -> Result<Response<DebugLevelResponse>, Status> {
        Err(Status::unimplemented("debug_level")) // todo
    }

    async fn fee_report(
        &self,
        request: Request<FeeReportRequest>,
    ) -> Result<Response<FeeReportResponse>, Status> {
        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(e.to_string()))?;

        let report = RoutedPayment::get_fee_report(&mut conn)
            .map_err(|e| Status::internal(e.to_string()))?;

        let channels = self.channel_manager.list_channels();
        let channel_fees = channels
            .into_iter()
            .map(|c| {
                let config = c.config.expect("safe after ldk 0.0.109");
                ChannelFeeReport {
                    chan_id: c.short_channel_id.unwrap(),
                    channel_point: c.funding_txo.map(|x| x.to_string()).unwrap_or_default(),
                    base_fee_msat: config.forwarding_fee_base_msat as i64,
                    fee_per_mil: config.forwarding_fee_proportional_millionths as i64,
                    fee_rate: (config.forwarding_fee_proportional_millionths as f64
                        / 1_000_000_f64),
                }
            })
            .collect();

        let response = FeeReportResponse {
            channel_fees,
            day_fee_sum: report.daily_fee_earned_msat as u64,
            week_fee_sum: report.weekly_fee_earned_msat as u64,
            month_fee_sum: report.monthly_fee_earned_msat as u64,
        };

        Ok(Response::new(response))
    }

    async fn update_channel_policy(
        &self,
        request: Request<PolicyUpdateRequest>,
    ) -> Result<Response<PolicyUpdateResponse>, Status> {
        Err(Status::unimplemented("update_channel_policy")) // todo
    }

    async fn forwarding_history(
        &self,
        request: Request<ForwardingHistoryRequest>,
    ) -> Result<Response<ForwardingHistoryResponse>, Status> {
        let req = request.into_inner();
        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(e.to_string()))?;

        let start = if req.start_time > 0 {
            Some(req.start_time as i64)
        } else {
            None
        };
        let end = if req.end_time > 0 {
            Some(req.end_time as i64)
        } else {
            None
        };

        let routed_payments = RoutedPayment::get_routed_payments(&mut conn, start, end)
            .map_err(|e| Status::internal(e.to_string()))?;

        let events: Vec<ForwardingEvent> = routed_payments
            .into_iter()
            .map(|p| {
                let amt_in_msat = (p.amount_forwarded + p.fee_earned_msat) as u64;
                let amt_out_msat = p.amount_forwarded as u64;
                ForwardingEvent {
                    fee_msat: p.fee_earned_msat as u64,
                    amt_in_msat,
                    amt_out_msat,
                    timestamp_ns: p.created_at.timestamp_nanos() as u64,
                    peer_alias_in: "".to_string(),
                    timestamp: p.created_at.timestamp() as u64,
                    chan_id_in: 0,
                    chan_id_out: 0,
                    amt_in: amt_in_msat / 1_000,
                    amt_out: amt_out_msat / 1_000,
                    fee: (p.fee_earned_msat / 1_000) as u64,
                    peer_alias_out: "".to_string(),
                }
            })
            .collect();

        let response = ForwardingHistoryResponse {
            forwarding_events: events,
            last_offset_index: 0,
        };

        Ok(Response::new(response))
    }

    async fn export_channel_backup(
        &self,
        request: Request<ExportChannelBackupRequest>,
    ) -> Result<Response<ChannelBackup>, Status> {
        Err(Status::unimplemented("export_channel_backup")) // todo
    }

    async fn export_all_channel_backups(
        &self,
        request: Request<ChanBackupExportRequest>,
    ) -> Result<Response<ChanBackupSnapshot>, Status> {
        Err(Status::unimplemented("export_all_channel_backups")) // todo
    }

    async fn verify_chan_backup(
        &self,
        request: Request<ChanBackupSnapshot>,
    ) -> Result<Response<VerifyChanBackupResponse>, Status> {
        Err(Status::unimplemented("verify_chan_backup")) // todo
    }

    async fn restore_channel_backups(
        &self,
        request: Request<RestoreChanBackupRequest>,
    ) -> Result<Response<RestoreBackupResponse>, Status> {
        Err(Status::unimplemented("restore_channel_backups")) // todo
    }

    type SubscribeChannelBackupsStream = ReceiverStream<Result<ChanBackupSnapshot, Status>>;

    async fn subscribe_channel_backups(
        &self,
        request: Request<ChannelBackupSubscription>,
    ) -> Result<Response<Self::SubscribeChannelBackupsStream>, Status> {
        Err(Status::unimplemented("subscribe_channel_backups")) // todo
    }

    async fn bake_macaroon(
        &self,
        request: Request<BakeMacaroonRequest>,
    ) -> Result<Response<BakeMacaroonResponse>, Status> {
        Err(Status::unimplemented("bake_macaroon")) // todo
    }

    async fn list_macaroon_i_ds(
        &self,
        request: Request<ListMacaroonIDsRequest>,
    ) -> Result<Response<ListMacaroonIDsResponse>, Status> {
        Err(Status::unimplemented("list_macaroon_i_ds")) // todo
    }

    async fn delete_macaroon_id(
        &self,
        request: Request<DeleteMacaroonIdRequest>,
    ) -> Result<Response<DeleteMacaroonIdResponse>, Status> {
        Err(Status::unimplemented("delete_macaroon_id")) // todo
    }

    async fn list_permissions(
        &self,
        request: Request<ListPermissionsRequest>,
    ) -> Result<Response<ListPermissionsResponse>, Status> {
        Err(Status::unimplemented("list_permissions")) // todo
    }

    async fn check_macaroon_permissions(
        &self,
        request: Request<CheckMacPermRequest>,
    ) -> Result<Response<CheckMacPermResponse>, Status> {
        Err(Status::unimplemented("check_macaroon_permissions")) // todo
    }

    type RegisterRPCMiddlewareStream = ReceiverStream<Result<RpcMiddlewareRequest, Status>>;

    async fn register_rpc_middleware(
        &self,
        request: Request<Streaming<RpcMiddlewareResponse>>,
    ) -> Result<Response<Self::RegisterRPCMiddlewareStream>, Status> {
        Err(Status::unimplemented("register_rpc_middleware")) // todo
    }

    async fn send_custom_message(
        &self,
        request: Request<SendCustomMessageRequest>,
    ) -> Result<Response<SendCustomMessageResponse>, Status> {
        Err(Status::unimplemented("send_custom_message")) // todo
    }

    type SubscribeCustomMessagesStream = ReceiverStream<Result<CustomMessage, Status>>;

    async fn subscribe_custom_messages(
        &self,
        request: Request<SubscribeCustomMessagesRequest>,
    ) -> Result<Response<Self::SubscribeCustomMessagesStream>, Status> {
        Err(Status::unimplemented("subscribe_custom_messages")) // todo
    }

    async fn list_aliases(
        &self,
        request: Request<ListAliasesRequest>,
    ) -> Result<Response<ListAliasesResponse>, Status> {
        Err(Status::unimplemented("list_aliases")) // todo
    }

    async fn lookup_htlc_resolution(
        &self,
        request: Request<LookupHtlcResolutionRequest>,
    ) -> Result<Response<LookupHtlcResolutionResponse>, Status> {
        Err(Status::unimplemented("lookup_htlc_resolution")) // todo
    }
}

#[tonic::async_trait]
impl Invoices for Node {
    type SubscribeSingleInvoiceStream = ReceiverStream<Result<Invoice, Status>>;

    async fn subscribe_single_invoice(
        &self,
        request: Request<SubscribeSingleInvoiceRequest>,
    ) -> Result<Response<Self::SubscribeSingleInvoiceStream>, Status> {
        let req = request.into_inner();
        if req.r_hash.len() != 32 {
            return Err(Status::invalid_argument("r_hash must be 32 bytes"));
        }

        let mut node_rx = self.invoice_broadcast.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Invoice, Status>>(128);
        tokio::spawn(async move {
            while let Ok(item) = node_rx.recv().await {
                if tx.is_closed() {
                    break;
                }

                // check if the invoice matches
                if item.payment_hash().to_vec() != req.r_hash {
                    continue;
                }

                let close = matches!(item.status(), InvoiceStatus::Expired | InvoiceStatus::Paid);

                // todo htlcs
                let invoice = receive_to_lnrpc_invoice(item);
                tx.send(Ok(invoice)).await.unwrap();

                if close {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn cancel_invoice(
        &self,
        request: Request<CancelInvoiceMsg>,
    ) -> Result<Response<CancelInvoiceResp>, Status> {
        let req = request.into_inner();

        let payment_hash = lightning::ln::PaymentHash(
            req.payment_hash
                .try_into()
                .map_err(|_| Status::internal("Invalid payment hash"))?,
        );

        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(e.to_string()))?;
        let _ = Receive::mark_as_canceled(&mut conn, payment_hash.0)
            .map_err(|e| Status::internal(e.to_string()))?;

        self.channel_manager.fail_htlc_backwards(&payment_hash);

        Ok(Response::new(CancelInvoiceResp {}))
    }

    async fn add_hold_invoice(
        &self,
        request: Request<AddHoldInvoiceRequest>,
    ) -> Result<Response<AddHoldInvoiceResp>, Status> {
        Err(Status::unimplemented("add_hold_invoice")) // todo
    }

    async fn settle_invoice(
        &self,
        request: Request<SettleInvoiceMsg>,
    ) -> Result<Response<SettleInvoiceResp>, Status> {
        let req = request.into_inner();

        let preimage = lightning::ln::PaymentPreimage(
            req.preimage
                .try_into()
                .map_err(|_| Status::internal("Invalid payment hash"))?,
        );

        // todo should check if the HTLC is still held

        self.channel_manager.claim_funds(preimage);

        Ok(Response::new(SettleInvoiceResp {}))
    }

    async fn lookup_invoice_v2(
        &self,
        _: Request<LookupInvoiceMsg>,
    ) -> Result<Response<Invoice>, Status> {
        Err(Status::unimplemented("lookup_invoice_v2")) // todo
    }
}

#[tonic::async_trait]
impl routerrpc::router_server::Router for Node {
    type SendPaymentV2Stream = ReceiverStream<Result<Payment, Status>>;

    async fn send_payment_v2(
        &self,
        request: Request<SendPaymentRequest>,
    ) -> Result<Response<Self::SendPaymentV2Stream>, Status> {
        log_trace!(self.logger, "send_payment_v2");
        let req = request.into_inner();

        if req.timeout_seconds == 0 {
            return Err(Status::invalid_argument("timeout_seconds is required"));
        }

        let amount_msats = if req.amt > 0 && req.amt_msat > 0 {
            return Err(Status::invalid_argument("Cannot have amt and amt_msat"));
        } else if req.amt_msat > 0 {
            Some(req.amt_msat as u64)
        } else if req.amt > 0 {
            Some(req.amt as u64 * 1_000)
        } else {
            None
        };

        let mut value_msats: i64;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Payment, Status>>(128);

        let invoice: Option<Bolt11Invoice> = if req.payment_request.is_empty() {
            None
        } else {
            Some(
                Bolt11Invoice::from_str(&req.payment_request)
                    .map_err(|e| Status::invalid_argument(format!("Invalid invoice: {e:?}")))?,
            )
        };

        let (hash, onion, route_params, pk) = if invoice.is_none() {
            if amount_msats.is_none() {
                return Err(Status::invalid_argument("amt or amt_msat is required"));
            }
            let amount_msats = amount_msats.unwrap();

            let pk = PublicKey::from_slice(&req.dest)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;

            let tlvs = req.dest_custom_records.into_iter().collect_vec();
            let onion = RecipientOnionFields::spontaneous_empty()
                .with_custom_tlvs(tlvs)
                .map_err(|e| Status::invalid_argument(format!("{e:?}")))?;

            let preimage = PaymentPreimage(self.keys_manager.get_secure_random_bytes());
            let payment_hash = sha256::Hash::hash(&preimage.0);

            let max_total_routing_fee_msat = if req.fee_limit_msat == 0 {
                None
            } else {
                Some(req.fee_limit_msat as u64)
            };

            let payment_params = PaymentParameters::for_keysend(pk, 40, false);
            let route_params: RouteParameters = RouteParameters {
                final_value_msat: amount_msats,
                payment_params,
                max_total_routing_fee_msat,
            };

            (
                lightning::ln::PaymentHash(payment_hash.to_byte_array()),
                onion,
                route_params,
                pk,
            )
        } else if let Some(invoice) = invoice.as_ref() {
            if invoice
                .amount_milli_satoshis()
                .is_some_and(|a| a != amount_msats.unwrap_or(a))
            {
                return Err(Status::invalid_argument(
                    "Invoice amount does not match request",
                ));
            }

            if invoice.network() != self.network {
                return Err(Status::invalid_argument("Invoice is on the wrong network"));
            }

            let pk = invoice.get_payee_pub_key();
            let (hash, onion, route_params) = match invoice.amount_milli_satoshis() {
                Some(_) => payment_parameters_from_invoice(invoice).expect("already checked"),
                None => match amount_msats {
                    Some(msats) => payment_parameters_from_zero_amount_invoice(invoice, msats)
                        .expect("already checked"),
                    None => {
                        return Err(Status::invalid_argument("Amount missing from request"));
                    }
                },
            };

            (hash, onion, route_params, pk)
        } else {
            return Err(Status::invalid_argument("Invalid payment params"));
        };

        let amount_msats = route_params.final_value_msat;

        let payment_id = PaymentId(hash.0);

        let mut conn = self
            .db_pool
            .get()
            .map_err(|e| Status::internal(format!("Error getting database connection: {e}")))?;
        models::payment::Payment::create(
            &mut conn,
            payment_id,
            hash,
            amount_msats as i64,
            Some(pk),
            invoice,
            None,
        )
        .map_err(|e| Status::internal(format!("Error creating payment db entry: {e}")))?;

        value_msats = amount_msats as i64;

        let mut pay_rx = self.payment_attempt_broadcast.subscribe();
        let payment_request = req.payment_request.clone();
        let tx = tx.clone();
        let self_clone = self.clone();
        tokio::spawn(async move {
            let mut attempts = 0;

            let start = SystemTime::now();

            let mut payment = Payment {
                payment_hash: hash.to_string(),
                value: value_msats / 1_000,
                creation_date: start
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
                fee: 0,
                payment_preimage: "".to_string(),
                value_sat: value_msats / 1_000,
                value_msat: value_msats,
                payment_request,
                status: PaymentStatus::InFlight as i32,
                fee_sat: 0,
                fee_msat: 0,
                creation_time_ns: start
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
                htlcs: vec![],
                payment_index: 0,
                failure_reason: PaymentFailureReason::FailureReasonNone as i32,
            };

            let node_id = self_clone.node_id();
            loop {
                if tx.is_closed() {
                    break;
                }

                if SystemTime::now().duration_since(start).unwrap().as_secs()
                    > req.timeout_seconds as u64
                {
                    payment.failure_reason = PaymentFailureReason::FailureReasonTimeout as i32;
                    payment.status = PaymentStatus::Failed as i32;

                    let _ = tx.send(Ok(payment)).await;
                    return;
                }

                let attempt_time = SystemTime::now();
                let route = self_clone
                    .router
                    .find_route(
                        &node_id,
                        &route_params,
                        None,
                        self_clone.channel_manager.compute_inflight_htlcs(),
                    )
                    .unwrap();

                attempts += 1;
                let htlcs = route
                    .paths
                    .iter()
                    .map(|path| {
                        let mut amt_to_forward_msat = path.final_value_msat() + path.fee_msat();
                        let hops = path
                            .hops
                            .iter()
                            .map(|hop| {
                                amt_to_forward_msat -= hop.fee_msat;
                                let fee_msat = if hop.fee_msat == path.final_value_msat() {
                                    0_i64
                                } else {
                                    hop.fee_msat as i64
                                };
                                Hop {
                                    chan_id: hop.short_channel_id,
                                    chan_capacity: amt_to_forward_msat as i64,
                                    amt_to_forward: amt_to_forward_msat as i64,
                                    fee: fee_msat / 1_000,
                                    expiry: hop.cltv_expiry_delta,
                                    amt_to_forward_msat: amt_to_forward_msat as i64,
                                    fee_msat,
                                    pub_key: hop.pubkey.to_string(),
                                    tlv_payload: true,
                                    mpp_record: None,
                                    amp_record: None,
                                    custom_records: Default::default(),
                                    metadata: vec![],
                                }
                            })
                            .collect();

                        HtlcAttempt {
                            attempt_id: attempts,
                            status: HtlcStatus::InFlight as i32,
                            route: Some(Route {
                                total_time_lock: path.final_cltv_expiry_delta().unwrap_or(0),
                                total_fees: path.fee_msat() as i64 / 1_000,
                                total_amt: value_msats / 1_000,
                                hops,
                                total_fees_msat: path.fee_msat() as i64,
                                total_amt_msat: value_msats,
                            }),
                            attempt_time_ns: attempt_time
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_nanos() as i64,
                            resolve_time_ns: 0,
                            failure: None,
                            preimage: vec![],
                        }
                    })
                    .collect();

                payment.htlcs = htlcs;

                match self_clone.channel_manager.send_payment_with_route(
                    route.clone(),
                    hash,
                    onion.clone(),
                    payment_id,
                ) {
                    Ok(_) => {
                        if !req.no_inflight_updates {
                            payment.fee_msat = route.get_total_fees() as i64;
                            payment.fee_sat = (route.get_total_fees() / 1000) as i64;

                            let _ = tx.send(Ok(payment.clone())).await;
                        }
                    }
                    Err(e) => {
                        tx.send(Err(Status::internal(format!(
                            "Error sending payment: {e:?}"
                        ))));
                        break;
                    }
                }

                loop {
                    if let Ok(attempt) = pay_rx.recv().await {
                        if attempt.payment_id() != payment_id {
                            continue;
                        }

                        match attempt {
                            PaymentAttempt::Successful { path, .. } => {
                                let res = self_clone
                                    .await_payment(payment_id, 60)
                                    .await
                                    .expect("Payment should have succeeded");
                                payment.fee_msat = path.fee_msat() as i64;
                                payment.fee_sat = (path.fee_msat() / 1000) as i64;
                                payment.status = PaymentStatus::Succeeded as i32;
                                payment.payment_preimage =
                                    res.preimage().map(hex::encode).unwrap_or_default();
                                let resolve_time_ns = SystemTime::now()
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .unwrap()
                                    .as_nanos();
                                payment.htlcs.iter_mut().for_each(|htlc| {
                                    htlc.status = HtlcStatus::Succeeded as i32;
                                    htlc.resolve_time_ns = resolve_time_ns as i64;
                                });

                                log_info!(self_clone.logger, "Successful payment: {payment:?}");

                                let _ = tx.send(Ok(payment)).await;

                                return;
                            }
                            PaymentAttempt::Failed {
                                failure,
                                path,
                                payment_failed_permanently,
                                ..
                            } => {
                                if payment_failed_permanently {
                                    payment.fee_msat = path.fee_msat() as i64;
                                    payment.fee_sat = (path.fee_msat() / 1000) as i64;
                                    payment.status = PaymentStatus::Failed as i32;
                                    payment.failure_reason =
                                        PaymentFailureReason::FailureReasonNoRoute as i32;

                                    payment.htlcs.iter_mut().for_each(|htlc| {
                                        htlc.status = HtlcStatus::Failed as i32;
                                        htlc.failure = Some(Failure {
                                            code: FailureCode::UnknownFailure as i32,
                                            channel_update: None,
                                            htlc_msat: path.final_value_msat(),
                                            onion_sha_256: vec![],
                                            cltv_expiry: 0,
                                            flags: 0,
                                            failure_source_index: 0,
                                            height: 0,
                                        });
                                    });

                                    let _ = tx.send(Ok(payment.clone())).await;
                                }
                            }
                        }
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type TrackPaymentV2Stream = ReceiverStream<Result<Payment, Status>>;

    async fn track_payment_v2(
        &self,
        request: Request<TrackPaymentRequest>,
    ) -> Result<Response<Self::TrackPaymentV2Stream>, Status> {
        log_trace!(self.logger, "track_payment_v2");
        Err(Status::unimplemented("track_payment_v2")) // todo
    }

    type TrackPaymentsStream = ReceiverStream<Result<Payment, Status>>;

    async fn track_payments(
        &self,
        request: Request<TrackPaymentsRequest>,
    ) -> Result<Response<Self::TrackPaymentsStream>, Status> {
        Err(Status::unimplemented("track_payments")) // todo
    }

    async fn estimate_route_fee(
        &self,
        request: Request<RouteFeeRequest>,
    ) -> Result<Response<RouteFeeResponse>, Status> {
        Err(Status::unimplemented("estimate_route_fee")) // todo
    }

    async fn send_to_route(
        &self,
        request: Request<routerrpc::SendToRouteRequest>,
    ) -> Result<Response<SendToRouteResponse>, Status> {
        Err(Status::unimplemented("send_to_route")) // todo
    }

    async fn send_to_route_v2(
        &self,
        request: Request<routerrpc::SendToRouteRequest>,
    ) -> Result<Response<HtlcAttempt>, Status> {
        Err(Status::unimplemented("send_to_route_v2")) // todo
    }

    async fn reset_mission_control(
        &self,
        request: Request<ResetMissionControlRequest>,
    ) -> Result<Response<ResetMissionControlResponse>, Status> {
        Err(Status::unimplemented("reset_mission_control")) // todo
    }

    async fn query_mission_control(
        &self,
        request: Request<QueryMissionControlRequest>,
    ) -> Result<Response<QueryMissionControlResponse>, Status> {
        Err(Status::unimplemented("query_mission_control")) // todo
    }

    async fn x_import_mission_control(
        &self,
        request: Request<XImportMissionControlRequest>,
    ) -> Result<Response<XImportMissionControlResponse>, Status> {
        Err(Status::unimplemented("x_import_mission_control")) // todo
    }

    async fn get_mission_control_config(
        &self,
        request: Request<GetMissionControlConfigRequest>,
    ) -> Result<Response<GetMissionControlConfigResponse>, Status> {
        Err(Status::unimplemented("get_mission_control_config")) // todo
    }

    async fn set_mission_control_config(
        &self,
        request: Request<SetMissionControlConfigRequest>,
    ) -> Result<Response<SetMissionControlConfigResponse>, Status> {
        Err(Status::unimplemented("set_mission_control_config")) // todo
    }

    async fn query_probability(
        &self,
        request: Request<QueryProbabilityRequest>,
    ) -> Result<Response<QueryProbabilityResponse>, Status> {
        Err(Status::unimplemented("query_probability")) // todo
    }

    async fn build_route(
        &self,
        request: Request<BuildRouteRequest>,
    ) -> Result<Response<BuildRouteResponse>, Status> {
        Err(Status::unimplemented("build_route")) // todo
    }

    type SubscribeHtlcEventsStream = ReceiverStream<Result<HtlcEvent, Status>>;

    async fn subscribe_htlc_events(
        &self,
        request: Request<SubscribeHtlcEventsRequest>,
    ) -> Result<Response<Self::SubscribeHtlcEventsStream>, Status> {
        Err(Status::unimplemented("subscribe_htlc_events")) // todo
    }

    type SendPaymentStream = ReceiverStream<Result<routerrpc::PaymentStatus, Status>>;

    async fn send_payment(
        &self,
        request: Request<SendPaymentRequest>,
    ) -> Result<Response<Self::SendPaymentStream>, Status> {
        Err(Status::unimplemented("send_payment"))
    }

    type TrackPaymentStream = ReceiverStream<Result<routerrpc::PaymentStatus, Status>>;

    async fn track_payment(
        &self,
        request: Request<TrackPaymentRequest>,
    ) -> Result<Response<Self::TrackPaymentStream>, Status> {
        Err(Status::unimplemented("track_payment")) // todo
    }

    type HtlcInterceptorStream = ReceiverStream<Result<ForwardHtlcInterceptRequest, Status>>;

    async fn htlc_interceptor(
        &self,
        request: Request<Streaming<ForwardHtlcInterceptResponse>>,
    ) -> Result<Response<Self::HtlcInterceptorStream>, Status> {
        Err(Status::unimplemented("htlc_interceptor")) // todo
    }

    async fn update_chan_status(
        &self,
        request: Request<UpdateChanStatusRequest>,
    ) -> Result<Response<UpdateChanStatusResponse>, Status> {
        Err(Status::unimplemented("update_chan_status")) // todo
    }
}

#[tonic::async_trait]
impl Signer for Node {
    async fn sign_output_raw(
        &self,
        request: Request<SignReq>,
    ) -> Result<Response<SignResp>, Status> {
        Err(Status::unimplemented("sign_output_raw")) // todo
    }

    async fn compute_input_script(
        &self,
        request: Request<SignReq>,
    ) -> Result<Response<InputScriptResp>, Status> {
        Err(Status::unimplemented("compute_input_script")) // todo
    }

    async fn sign_message(
        &self,
        request: Request<SignMessageReq>,
    ) -> Result<Response<SignMessageResp>, Status> {
        Err(Status::unimplemented("sign_message")) // todo
    }

    async fn verify_message(
        &self,
        request: Request<VerifyMessageReq>,
    ) -> Result<Response<VerifyMessageResp>, Status> {
        Err(Status::unimplemented("verify_message")) // todo
    }

    async fn derive_shared_key(
        &self,
        request: Request<SharedKeyRequest>,
    ) -> Result<Response<SharedKeyResponse>, Status> {
        Err(Status::unimplemented("derive_shared_key")) // todo
    }

    async fn mu_sig2_combine_keys(
        &self,
        request: Request<MuSig2CombineKeysRequest>,
    ) -> Result<Response<MuSig2CombineKeysResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_combine_keys")) // todo
    }

    async fn mu_sig2_create_session(
        &self,
        request: Request<MuSig2SessionRequest>,
    ) -> Result<Response<MuSig2SessionResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_create_session")) // todo
    }

    async fn mu_sig2_register_nonces(
        &self,
        request: Request<MuSig2RegisterNoncesRequest>,
    ) -> Result<Response<MuSig2RegisterNoncesResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_register_nonces")) // todo
    }

    async fn mu_sig2_sign(
        &self,
        request: Request<MuSig2SignRequest>,
    ) -> Result<Response<MuSig2SignResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_sign")) // todo
    }

    async fn mu_sig2_combine_sig(
        &self,
        request: Request<MuSig2CombineSigRequest>,
    ) -> Result<Response<MuSig2CombineSigResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_combine_sig")) // todo
    }

    async fn mu_sig2_cleanup(
        &self,
        request: Request<MuSig2CleanupRequest>,
    ) -> Result<Response<MuSig2CleanupResponse>, Status> {
        Err(Status::unimplemented("mu_sig2_cleanup")) // todo
    }
}

#[tonic::async_trait]
impl WalletKit for Node {
    async fn list_unspent(
        &self,
        request: Request<crate::walletrpc::ListUnspentRequest>,
    ) -> Result<Response<crate::walletrpc::ListUnspentResponse>, Status> {
        let crate::walletrpc::ListUnspentRequest {
            min_confs,
            max_confs,
            account,
            unconfirmed_only,
        } = request.into_inner();

        if !account.is_empty() || account != "default" {
            return Err(Status::invalid_argument("account is not supported"));
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
                let confs = match u.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => {
                        current_height as i32 - anchor.block_id.height as i32 + 1
                    }
                    ChainPosition::Unconfirmed { .. } => 0,
                };

                if unconfirmed_only && confs > 0 {
                    return None;
                }

                if confs >= min_confs && confs <= max_confs {
                    let address_type = if u.txout.script_pubkey.is_p2tr() {
                        AddressType::TaprootPubkey
                    } else if u.txout.script_pubkey.is_p2wpkh() {
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
                        amount_sat: u.txout.value.to_sat() as i64,
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

        Ok(Response::new(crate::walletrpc::ListUnspentResponse {
            utxos,
        }))
    }

    async fn lease_output(
        &self,
        request: Request<LeaseOutputRequest>,
    ) -> Result<Response<LeaseOutputResponse>, Status> {
        Err(Status::unimplemented("lease_output")) // todo
    }

    async fn release_output(
        &self,
        request: Request<ReleaseOutputRequest>,
    ) -> Result<Response<ReleaseOutputResponse>, Status> {
        Err(Status::unimplemented("release_output")) // todo
    }

    async fn list_leases(
        &self,
        request: Request<ListLeasesRequest>,
    ) -> Result<Response<ListLeasesResponse>, Status> {
        Err(Status::unimplemented("list_leases")) // todo
    }

    async fn derive_next_key(
        &self,
        request: Request<KeyReq>,
    ) -> Result<Response<crate::signrpc::KeyDescriptor>, Status> {
        Err(Status::unimplemented("derive_next_key")) // todo
    }

    async fn derive_key(
        &self,
        request: Request<crate::signrpc::KeyLocator>,
    ) -> Result<Response<crate::signrpc::KeyDescriptor>, Status> {
        Err(Status::unimplemented("derive_key")) // todo
    }

    async fn next_addr(
        &self,
        request: Request<AddrRequest>,
    ) -> Result<Response<AddrResponse>, Status> {
        let req = request.into_inner();

        if req.account.is_empty() || req.account != "default" {
            return Err(Status::invalid_argument("account is not supported"));
        }

        if req.r#type != AddressType::TaprootPubkey as i32 {
            return Err(Status::invalid_argument("address type is not supported"));
        }

        let info = if req.change {
            self.wallet
                .get_change_address()
                .map_err(|e| Status::internal(e.to_string()))?
        } else {
            self.wallet
                .get_new_address()
                .map_err(|e| Status::internal(e.to_string()))?
        };

        let response = AddrResponse {
            addr: info.address.to_string(),
        };

        Ok(Response::new(response))
    }

    async fn list_accounts(
        &self,
        request: Request<ListAccountsRequest>,
    ) -> Result<Response<ListAccountsResponse>, Status> {
        Err(Status::unimplemented("list_accounts")) // todo
    }

    async fn required_reserve(
        &self,
        request: Request<RequiredReserveRequest>,
    ) -> Result<Response<RequiredReserveResponse>, Status> {
        Err(Status::unimplemented("required_reserve")) // todo
    }

    async fn list_addresses(
        &self,
        request: Request<ListAddressesRequest>,
    ) -> Result<Response<ListAddressesResponse>, Status> {
        Err(Status::unimplemented("list_addresses")) // todo
    }

    async fn sign_message_with_addr(
        &self,
        request: Request<SignMessageWithAddrRequest>,
    ) -> Result<Response<SignMessageWithAddrResponse>, Status> {
        Err(Status::unimplemented("sign_message_with_addr")) // todo
    }

    async fn verify_message_with_addr(
        &self,
        request: Request<VerifyMessageWithAddrRequest>,
    ) -> Result<Response<VerifyMessageWithAddrResponse>, Status> {
        Err(Status::unimplemented("verify_message_with_addr")) // todo
    }

    async fn import_account(
        &self,
        request: Request<ImportAccountRequest>,
    ) -> Result<Response<ImportAccountResponse>, Status> {
        Err(Status::unimplemented("import_account")) // todo
    }

    async fn import_public_key(
        &self,
        request: Request<ImportPublicKeyRequest>,
    ) -> Result<Response<ImportPublicKeyResponse>, Status> {
        Err(Status::unimplemented("import_public_key")) // todo
    }

    async fn import_tapscript(
        &self,
        request: Request<ImportTapscriptRequest>,
    ) -> Result<Response<ImportTapscriptResponse>, Status> {
        Err(Status::unimplemented("import_tapscript")) // todo
    }

    async fn publish_transaction(
        &self,
        request: Request<crate::walletrpc::Transaction>,
    ) -> Result<Response<PublishResponse>, Status> {
        let req = request.into_inner();

        let tx: bitcoin::transaction::Transaction =
            deserialize(&req.tx_hex).map_err(|e| Status::invalid_argument(e.to_string()))?;

        let txid = self
            .wallet
            .broadcast_transaction(tx)
            .map_err(|e| Status::internal(e.to_string()))?;

        let resp = PublishResponse {
            publish_error: "".to_string(),
        };
        Ok(Response::new(resp))
    }

    async fn send_outputs(
        &self,
        request: Request<SendOutputsRequest>,
    ) -> Result<Response<SendOutputsResponse>, Status> {
        let req = request.into_inner();

        let fee_rate = if req.sat_per_kw != 0 {
            Some(FeeRate::from_sat_per_kwu(req.sat_per_kw as u64))
        } else {
            None
        };

        let outputs = req
            .outputs
            .into_iter()
            .map(|o| TxOut {
                value: bitcoin::Amount::from_sat(o.value as u64),
                script_pubkey: ScriptBuf::from(o.pk_script.to_vec()),
            })
            .collect();

        let txid = self
            .wallet
            .send_to_outputs(outputs, fee_rate)
            .map_err(|e| Status::internal(e.to_string()))?;

        let tx = self
            .wallet
            .get_transaction(txid)
            .map_err(|e| Status::internal(e.to_string()))?;
        if let Some(tx) = tx {
            let resp = SendOutputsResponse {
                raw_tx: serialize(&tx.transaction),
            };
            Ok(Response::new(resp))
        } else {
            Err(Status::internal("Could not get transaction"))
        }
    }

    async fn estimate_fee(
        &self,
        request: Request<crate::walletrpc::EstimateFeeRequest>,
    ) -> Result<Response<crate::walletrpc::EstimateFeeResponse>, Status> {
        let req = request.into_inner();

        let conf_target = if req.conf_target == 0 {
            1_u16
        } else {
            req.conf_target as u16
        };

        let res = self
            .bitcoind
            .estimate_smart_fee(conf_target, Some(EstimateMode::Economical))
            .map_err(|e| Status::internal(e.to_string()))?;

        if let Some(per_kb) = res.fee_rate {
            let fee_rate = FeeRate::from_sat_per_vb_unchecked(per_kb.to_sat() / 1_000);
            let response = crate::walletrpc::EstimateFeeResponse {
                sat_per_kw: fee_rate.to_sat_per_kwu() as i64,
            };

            Ok(Response::new(response))
        } else {
            Err(Status::internal("Error getting fee rate"))
        }
    }

    async fn pending_sweeps(
        &self,
        request: Request<PendingSweepsRequest>,
    ) -> Result<Response<PendingSweepsResponse>, Status> {
        Err(Status::unimplemented("pending_sweeps")) // todo
    }

    async fn bump_fee(
        &self,
        request: Request<BumpFeeRequest>,
    ) -> Result<Response<BumpFeeResponse>, Status> {
        Err(Status::unimplemented("bump_fee")) // todo
    }

    async fn list_sweeps(
        &self,
        request: Request<ListSweepsRequest>,
    ) -> Result<Response<ListSweepsResponse>, Status> {
        Err(Status::unimplemented("list_sweeps")) // todo
    }

    async fn label_transaction(
        &self,
        request: Request<LabelTransactionRequest>,
    ) -> Result<Response<LabelTransactionResponse>, Status> {
        Err(Status::unimplemented("label_transaction")) // todo
    }

    async fn fund_psbt(
        &self,
        request: Request<FundPsbtRequest>,
    ) -> Result<Response<FundPsbtResponse>, Status> {
        Err(Status::unimplemented("fund_psbt")) // todo
    }

    async fn sign_psbt(
        &self,
        request: Request<SignPsbtRequest>,
    ) -> Result<Response<SignPsbtResponse>, Status> {
        let req = request.into_inner();

        let unsigned = Psbt::deserialize(&req.funded_psbt)
            .map_err(|e| Status::invalid_argument(format!("Could not deserialize psbt: {e:?}")))?;

        let psbt = self
            .wallet
            .sign_psbt(unsigned.clone())
            .map_err(|e| Status::invalid_argument(format!("Could not sign psbt: {e:?}")))?;

        // find the indexes of the inputs that we signed
        let signed_inputs = psbt
            .inputs
            .iter()
            .enumerate()
            .filter(|(idx, input)| **input != unsigned.inputs[*idx])
            .map(|(index, _)| index as u32)
            .collect();

        let resp = SignPsbtResponse {
            signed_psbt: psbt.serialize(),
            signed_inputs,
        };
        Ok(Response::new(resp))
    }

    async fn finalize_psbt(
        &self,
        request: Request<FinalizePsbtRequest>,
    ) -> Result<Response<FinalizePsbtResponse>, Status> {
        let req = request.into_inner();

        let psbt = Psbt::deserialize(&req.funded_psbt)
            .map_err(|e| Status::invalid_argument(format!("Could not deserialize psbt: {e:?}")))?;

        let psbt = self
            .wallet
            .sign_psbt(psbt)
            .map_err(|e| Status::invalid_argument(format!("Could not sign psbt: {e:?}")))?;

        let signed_psbt = psbt.serialize();

        let raw_final_tx = psbt.extract_tx().map(|tx| serialize(&tx)).unwrap_or(vec![]);

        let resp = FinalizePsbtResponse {
            signed_psbt,
            raw_final_tx,
        };
        Ok(Response::new(resp))
    }
}

#[tonic::async_trait]
impl Offers for Node {
    async fn pay_offer(
        &self,
        request: Request<PayOfferRequest>,
    ) -> Result<Response<PayOfferResponse>, Status> {
        log_info!(self.logger, "Paying offer!");
        let req = request.into_inner();

        let offer = Offer::from_str(&req.offer)
            .map_err(|e| Status::invalid_argument(format!("Invalid offer: {e:?}")))?;

        let payment = self
            .pay_offer_with_timeout(offer, req.amount, None)
            .await
            .map_err(|e| Status::internal(format!("Error paying offer: {e:?}")))?;

        Ok(Response::new(PayOfferResponse {
            payment_preimage: payment.preimage().map(hex::encode).unwrap_or_default(),
        }))
    }

    async fn get_invoice(
        &self,
        request: Request<GetInvoiceRequest>,
    ) -> Result<Response<GetInvoiceResponse>, Status> {
        Err(Status::unimplemented("get_invoice")) // todo
    }

    async fn decode_invoice(
        &self,
        request: Request<DecodeInvoiceRequest>,
    ) -> Result<Response<Bolt12InvoiceContents>, Status> {
        let req = request.into_inner();

        let invoice_string: Vec<u8> = hex::decode(req.invoice)
            .map_err(|e| Status::invalid_argument(format!("Invalid invoice hex: {e:?}")))?;
        let invoice = Bolt12Invoice::try_from(invoice_string)
            .map_err(|e| Status::invalid_argument(format!("Invalid invoice: {e:?}")))?;

        let reply: Bolt12InvoiceContents = generate_bolt12_invoice_contents(&invoice);

        Ok(Response::new(reply))
    }

    async fn pay_invoice(
        &self,
        request: Request<PayInvoiceRequest>,
    ) -> Result<Response<PayInvoiceResponse>, Status> {
        Err(Status::unimplemented("pay_invoice")) // todo
    }
}

fn receive_to_lnrpc_invoice(invoice: Receive) -> Invoice {
    let bolt11 = invoice.bolt11();
    let state: InvoiceState = match invoice.status() {
        InvoiceStatus::Pending => InvoiceState::Open,
        InvoiceStatus::Expired => InvoiceState::Canceled,
        InvoiceStatus::Held => InvoiceState::Accepted,
        InvoiceStatus::Paid => InvoiceState::Settled,
        InvoiceStatus::Canceled => InvoiceState::Canceled,
    };

    let route_hints = bolt11
        .as_ref()
        .map(|b| b.route_hints())
        .unwrap_or_default()
        .into_iter()
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
        .collect::<Vec<_>>();

    let amt_paid_msat = if state == InvoiceState::Settled {
        invoice.amount_msats.unwrap_or_default()
    } else {
        0
    };

    let (memo, description_hash) = match bolt11.as_ref().map(|b| b.description()) {
        Some(Bolt11InvoiceDescription::Direct(desc)) => (desc.to_string(), vec![]),
        Some(Bolt11InvoiceDescription::Hash(hash)) => {
            (String::new(), hash.0.to_byte_array().to_vec())
        }
        None => (String::new(), vec![]),
    };

    let fallback_addr = bolt11
        .as_ref()
        .and_then(|b| b.fallback_addresses().first().map(|a| a.to_string()));

    let value_msat = bolt11
        .as_ref()
        .and_then(|b| b.amount_milli_satoshis())
        .unwrap_or_default() as i64;
    Invoice {
        memo,
        r_preimage: invoice.preimage().map(|p| p.to_vec()).unwrap_or_default(),
        r_hash: invoice.payment_hash().to_vec(),
        value: value_msat / 1_000,
        value_msat,
        settled: state == InvoiceState::Settled,
        creation_date: invoice.creation_date(),
        settle_date: invoice.settled_at().unwrap_or_default(),
        payment_request: bolt11.as_ref().map(|b| b.to_string()).unwrap_or_default(),
        description_hash,
        expiry: bolt11
            .as_ref()
            .map(|b| b.expiry_time().as_secs())
            .unwrap_or_default() as i64,
        fallback_addr: fallback_addr.unwrap_or_default(),
        cltv_expiry: bolt11
            .as_ref()
            .map(|b| b.min_final_cltv_expiry_delta())
            .unwrap_or_default(),
        private: !route_hints.is_empty(),
        route_hints,
        add_index: invoice.id as u64,
        settle_index: invoice.id as u64,
        amt_paid: 0,
        amt_paid_sat: amt_paid_msat / 1_000,
        amt_paid_msat,
        state: state.into(),
        htlcs: vec![],
        features: Default::default(),
        is_keysend: bolt11.is_none(),
        payment_addr: bolt11
            .as_ref()
            .map(|b| b.payment_secret().0.to_vec())
            .unwrap_or_default(),
        is_amp: false,
        amp_invoice_state: Default::default(),
    }
}

fn get_output_type(spk: &ScriptBuf) -> OutputScriptType {
    if spk.is_p2pkh() {
        OutputScriptType::ScriptTypePubkeyHash
    } else if spk.is_p2sh() {
        OutputScriptType::ScriptTypeScriptHash
    } else if spk.is_p2wpkh() {
        OutputScriptType::ScriptTypeWitnessV0PubkeyHash
    } else if spk.is_p2wsh() {
        OutputScriptType::ScriptTypeWitnessV0ScriptHash
    } else if spk.is_p2tr() {
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

fn generate_bolt12_invoice_contents(invoice: &Bolt12Invoice) -> Bolt12InvoiceContents {
    Bolt12InvoiceContents {
        chain: invoice.chain().to_string(),
        quantity: invoice.quantity(),
        amount_msats: invoice.amount_msats(),
        description: invoice
            .description()
            .map(|description| description.to_string()),
        payment_hash: Some(crate::lndkrpc::PaymentHash {
            hash: invoice.payment_hash().0.to_vec(),
        }),
        created_at: invoice.created_at().as_secs() as i64,
        relative_expiry: invoice.relative_expiry().as_secs(),
        node_id: Some(convert_public_key(invoice.signing_pubkey())),
        signature: invoice.signature().to_string(),
        payment_paths: extract_payment_paths(invoice),
        features: convert_features(invoice.invoice_features().clone().encode()),
    }
}

fn encode_invoice_as_hex(invoice: &Bolt12Invoice) -> Result<String, Status> {
    let mut buffer = Vec::new();
    invoice
        .write(&mut buffer)
        .map_err(|e| Status::internal(format!("Error serializing invoice: {e}")))?;
    Ok(hex::encode(buffer))
}

fn extract_payment_paths(invoice: &Bolt12Invoice) -> Vec<PaymentPaths> {
    invoice
        .payment_paths()
        .iter()
        .map(|blinded_pay_info| PaymentPaths {
            blinded_pay_info: Some(convert_blinded_pay_info(&blinded_pay_info.payinfo)),
            blinded_path: Some(convert_blinded_path(blinded_pay_info)),
        })
        .collect()
}

fn convert_public_key(native_pub_key: PublicKey) -> lndkrpc::PublicKey {
    let pub_key_bytes = native_pub_key.encode();
    lndkrpc::PublicKey { key: pub_key_bytes }
}

fn convert_blinded_pay_info(native_info: &BlindedPayInfo) -> lndkrpc::BlindedPayInfo {
    lndkrpc::BlindedPayInfo {
        fee_base_msat: native_info.fee_base_msat,
        fee_proportional_millionths: native_info.fee_proportional_millionths,
        cltv_expiry_delta: native_info.cltv_expiry_delta as u32,
        htlc_minimum_msat: native_info.htlc_minimum_msat,
        htlc_maximum_msat: native_info.htlc_maximum_msat,
        features: convert_features(native_info.features.clone().encode()),
    }
}

fn convert_blinded_path(native_info: &BlindedPaymentPath) -> lndkrpc::BlindedPath {
    let introduction_node = match native_info.introduction_node() {
        IntroductionNode::NodeId(pubkey) => lndkrpc::IntroductionNode {
            node_id: Some(convert_public_key(*pubkey)),
            directed_short_channel_id: None,
        },
        IntroductionNode::DirectedShortChannelId(direction, scid) => {
            let rpc_direction = match direction {
                Direction::NodeOne => lndkrpc::Direction::NodeOne,
                Direction::NodeTwo => lndkrpc::Direction::NodeTwo,
            };

            lndkrpc::IntroductionNode {
                node_id: None,
                directed_short_channel_id: Some(lndkrpc::DirectedShortChannelId {
                    direction: rpc_direction.into(),
                    scid: *scid,
                }),
            }
        }
    };

    lndkrpc::BlindedPath {
        introduction_node: Some(introduction_node),
        blinding_point: Some(convert_public_key(native_info.blinding_point())),
        blinded_hops: native_info
            .blinded_hops()
            .iter()
            .map(|hop| lndkrpc::BlindedHop {
                blinded_node_id: Some(convert_public_key(hop.blinded_node_id)),
                encrypted_payload: hop.encrypted_payload.clone(),
            })
            .collect(),
    }
}

// Conversion function for FeatureBit.
// TODO: Converting the FeatureBits doesn't work quite properly right now.
fn feature_bit_from_id(feature_id: u8) -> Option<FeatureBit> {
    match feature_id {
        0 => Some(FeatureBit::DatalossProtectOpt),
        1 => Some(FeatureBit::DatalossProtectOpt),
        3 => Some(FeatureBit::InitialRouingSync),
        4 => Some(FeatureBit::UpfrontShutdownScriptReq),
        5 => Some(FeatureBit::UpfrontShutdownScriptOpt),
        6 => Some(FeatureBit::GossipQueriesReq),
        7 => Some(FeatureBit::GossipQueriesOpt),
        8 => Some(FeatureBit::TlvOnionReq),
        9 => Some(FeatureBit::TlvOnionOpt),
        10 => Some(FeatureBit::ExtGossipQueriesReq),
        11 => Some(FeatureBit::ExtGossipQueriesOpt),
        12 => Some(FeatureBit::StaticRemoteKeyReq),
        13 => Some(FeatureBit::StaticRemoteKeyOpt),
        14 => Some(FeatureBit::PaymentAddrReq),
        15 => Some(FeatureBit::PaymentAddrOpt),
        16 => Some(FeatureBit::MppReq),
        17 => Some(FeatureBit::MppOpt),
        18 => Some(FeatureBit::WumboChannelsReq),
        19 => Some(FeatureBit::WumboChannelsOpt),
        20 => Some(FeatureBit::AnchorsReq),
        21 => Some(FeatureBit::AnchorsOpt),
        22 => Some(FeatureBit::AnchorsZeroFeeHtlcReq),
        23 => Some(FeatureBit::AnchorsZeroFeeHtlcOpt),
        30 => Some(FeatureBit::AmpReq),
        31 => Some(FeatureBit::AmpOpt),
        _ => None,
    }
}

// Conversion function for features.
// TODO: Converting the FeatureBits doesn't work quite properly right now.
fn convert_features(features: Vec<u8>) -> Vec<i32> {
    features
        .iter()
        .filter_map(|&feature_id| feature_bit_from_id(feature_id))
        .map(|feature_bit| feature_bit as i32) // Cast enum variant to i32
        .collect()
}
