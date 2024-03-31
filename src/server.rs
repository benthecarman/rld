#![allow(deprecated)]
#![allow(unused)]

use crate::models::invoice::InvoiceStatus;
use crate::node::Node;
use crate::proto::channel_point::FundingTxid;
use crate::proto::invoice::InvoiceState;
use crate::proto::lightning_server::Lightning;
use crate::proto::pending_channels_response::{PendingChannel, PendingOpenChannel};
use crate::proto::*;
use bitcoin::ecdsa::Signature;
use bitcoin::hashes::{sha256::Hash as Sha256, Hash};
use bitcoin::secp256k1::{Message, PublicKey};
use bitcoin::{FeeRate, Network};
use lightning::ln::channelmanager::{PaymentId, Retry};
use lightning::sign::{NodeSigner, Recipient};
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
        todo!()
    }

    async fn estimate_fee(
        &self,
        request: Request<EstimateFeeRequest>,
    ) -> Result<Response<EstimateFeeResponse>, Status> {
        todo!()
    }

    async fn send_coins(
        &self,
        request: Request<SendCoinsRequest>,
    ) -> Result<Response<SendCoinsResponse>, Status> {
        todo!()
    }

    async fn list_unspent(
        &self,
        request: Request<ListUnspentRequest>,
    ) -> Result<Response<ListUnspentResponse>, Status> {
        todo!()
    }

    type SubscribeTransactionsStream = ReceiverStream<Result<Transaction, Status>>;

    async fn subscribe_transactions(
        &self,
        request: Request<GetTransactionsRequest>,
    ) -> Result<Response<Self::SubscribeTransactionsStream>, Status> {
        todo!()
    }

    async fn send_many(
        &self,
        request: Request<SendManyRequest>,
    ) -> Result<Response<SendManyResponse>, Status> {
        todo!()
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
        let msg = if req.single_hash {
            Sha256::hash(&req.msg)
        } else {
            let first = Sha256::hash(&req.msg);
            Sha256::hash(first.as_byte_array())
        };

        let message = Message::from(msg);
        let sig = self
            .secp
            .sign_ecdsa(&message, &self.keys_manager.get_node_secret_key());

        let response = SignMessageResponse {
            signature: sig.to_string(),
        };
        Ok(Response::new(response))
    }

    async fn verify_message(
        &self,
        request: Request<VerifyMessageRequest>,
    ) -> Result<Response<VerifyMessageResponse>, Status> {
        todo!()
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
        todo!()
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
                    commitment_type: 2, // todo handle anchors
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
        todo!()
    }

    type SubscribeChannelEventsStream = ReceiverStream<Result<ChannelEventUpdate, Status>>;

    async fn subscribe_channel_events(
        &self,
        request: Request<ChannelEventSubscription>,
    ) -> Result<Response<Self::SubscribeChannelEventsStream>, Status> {
        todo!()
    }

    async fn closed_channels(
        &self,
        request: Request<ClosedChannelsRequest>,
    ) -> Result<Response<ClosedChannelsResponse>, Status> {
        todo!()
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
        todo!()
    }

    async fn batch_open_channel(
        &self,
        request: Request<BatchOpenChannelRequest>,
    ) -> Result<Response<BatchOpenChannelResponse>, Status> {
        todo!()
    }

    async fn funding_state_step(
        &self,
        request: Request<FundingTransitionMsg>,
    ) -> Result<Response<FundingStateStepResp>, Status> {
        todo!()
    }

    type ChannelAcceptorStream = ReceiverStream<Result<ChannelAcceptRequest, Status>>;

    async fn channel_acceptor(
        &self,
        request: Request<Streaming<ChannelAcceptResponse>>,
    ) -> Result<Response<Self::ChannelAcceptorStream>, Status> {
        todo!()
    }

    type CloseChannelStream = ReceiverStream<Result<CloseStatusUpdate, Status>>;

    async fn close_channel(
        &self,
        request: Request<CloseChannelRequest>,
    ) -> Result<Response<Self::CloseChannelStream>, Status> {
        todo!()
    }

    async fn abandon_channel(
        &self,
        request: Request<AbandonChannelRequest>,
    ) -> Result<Response<AbandonChannelResponse>, Status> {
        todo!()
    }

    type SendPaymentStream = ReceiverStream<Result<SendResponse, Status>>;

    async fn send_payment(
        &self,
        request: Request<Streaming<SendRequest>>,
    ) -> Result<Response<Self::SendPaymentStream>, Status> {
        todo!()
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
        todo!()
    }

    async fn send_to_route_sync(
        &self,
        request: Request<SendToRouteRequest>,
    ) -> Result<Response<SendResponse>, Status> {
        todo!()
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

        let invoice = if req.description_hash.is_empty() {
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
            payment_request: invoice.to_string(),
            r_hash: invoice.payment_hash().to_byte_array().to_vec(),
            add_index: 0,
            payment_addr: invoice.payment_secret().0.to_vec(),
        };

        Ok(Response::new(response))
    }

    async fn list_invoices(
        &self,
        request: Request<ListInvoiceRequest>,
    ) -> Result<Response<ListInvoiceResponse>, Status> {
        todo!()
    }

    async fn lookup_invoice(
        &self,
        request: Request<PaymentHash>,
    ) -> Result<Response<Invoice>, Status> {
        todo!()
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
            route_hints: vec![],
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
        todo!()
    }

    async fn delete_payment(
        &self,
        request: Request<DeletePaymentRequest>,
    ) -> Result<Response<DeletePaymentResponse>, Status> {
        todo!()
    }

    async fn delete_all_payments(
        &self,
        request: Request<DeleteAllPaymentsRequest>,
    ) -> Result<Response<DeleteAllPaymentsResponse>, Status> {
        todo!()
    }

    async fn describe_graph(
        &self,
        request: Request<ChannelGraphRequest>,
    ) -> Result<Response<ChannelGraph>, Status> {
        todo!()
    }

    async fn get_node_metrics(
        &self,
        request: Request<NodeMetricsRequest>,
    ) -> Result<Response<NodeMetricsResponse>, Status> {
        todo!()
    }

    async fn get_chan_info(
        &self,
        request: Request<ChanInfoRequest>,
    ) -> Result<Response<ChannelEdge>, Status> {
        todo!()
    }

    async fn get_node_info(
        &self,
        request: Request<NodeInfoRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        todo!()
    }

    async fn query_routes(
        &self,
        request: Request<QueryRoutesRequest>,
    ) -> Result<Response<QueryRoutesResponse>, Status> {
        todo!()
    }

    async fn get_network_info(
        &self,
        request: Request<NetworkInfoRequest>,
    ) -> Result<Response<NetworkInfo>, Status> {
        todo!()
    }

    async fn stop_daemon(
        &self,
        request: Request<StopRequest>,
    ) -> Result<Response<StopResponse>, Status> {
        todo!()
    }

    type SubscribeChannelGraphStream = ReceiverStream<Result<GraphTopologyUpdate, Status>>;

    async fn subscribe_channel_graph(
        &self,
        request: Request<GraphTopologySubscription>,
    ) -> Result<Response<Self::SubscribeChannelGraphStream>, Status> {
        todo!()
    }

    async fn debug_level(
        &self,
        request: Request<DebugLevelRequest>,
    ) -> Result<Response<DebugLevelResponse>, Status> {
        todo!()
    }

    async fn fee_report(
        &self,
        request: Request<FeeReportRequest>,
    ) -> Result<Response<FeeReportResponse>, Status> {
        todo!()
    }

    async fn update_channel_policy(
        &self,
        request: Request<PolicyUpdateRequest>,
    ) -> Result<Response<PolicyUpdateResponse>, Status> {
        todo!()
    }

    async fn forwarding_history(
        &self,
        request: Request<ForwardingHistoryRequest>,
    ) -> Result<Response<ForwardingHistoryResponse>, Status> {
        todo!()
    }

    async fn export_channel_backup(
        &self,
        request: Request<ExportChannelBackupRequest>,
    ) -> Result<Response<ChannelBackup>, Status> {
        todo!()
    }

    async fn export_all_channel_backups(
        &self,
        request: Request<ChanBackupExportRequest>,
    ) -> Result<Response<ChanBackupSnapshot>, Status> {
        todo!()
    }

    async fn verify_chan_backup(
        &self,
        request: Request<ChanBackupSnapshot>,
    ) -> Result<Response<VerifyChanBackupResponse>, Status> {
        todo!()
    }

    async fn restore_channel_backups(
        &self,
        request: Request<RestoreChanBackupRequest>,
    ) -> Result<Response<RestoreBackupResponse>, Status> {
        todo!()
    }

    type SubscribeChannelBackupsStream = ReceiverStream<Result<ChanBackupSnapshot, Status>>;

    async fn subscribe_channel_backups(
        &self,
        request: Request<ChannelBackupSubscription>,
    ) -> Result<Response<Self::SubscribeChannelBackupsStream>, Status> {
        todo!()
    }

    async fn bake_macaroon(
        &self,
        request: Request<BakeMacaroonRequest>,
    ) -> Result<Response<BakeMacaroonResponse>, Status> {
        todo!()
    }

    async fn list_macaroon_i_ds(
        &self,
        request: Request<ListMacaroonIDsRequest>,
    ) -> Result<Response<ListMacaroonIDsResponse>, Status> {
        todo!()
    }

    async fn delete_macaroon_id(
        &self,
        request: Request<DeleteMacaroonIdRequest>,
    ) -> Result<Response<DeleteMacaroonIdResponse>, Status> {
        todo!()
    }

    async fn list_permissions(
        &self,
        request: Request<ListPermissionsRequest>,
    ) -> Result<Response<ListPermissionsResponse>, Status> {
        todo!()
    }

    async fn check_macaroon_permissions(
        &self,
        request: Request<CheckMacPermRequest>,
    ) -> Result<Response<CheckMacPermResponse>, Status> {
        todo!()
    }

    type RegisterRPCMiddlewareStream = ReceiverStream<Result<RpcMiddlewareRequest, Status>>;

    async fn register_rpc_middleware(
        &self,
        request: Request<Streaming<RpcMiddlewareResponse>>,
    ) -> Result<Response<Self::RegisterRPCMiddlewareStream>, Status> {
        todo!()
    }

    async fn send_custom_message(
        &self,
        request: Request<SendCustomMessageRequest>,
    ) -> Result<Response<SendCustomMessageResponse>, Status> {
        todo!()
    }

    type SubscribeCustomMessagesStream = ReceiverStream<Result<CustomMessage, Status>>;

    async fn subscribe_custom_messages(
        &self,
        request: Request<SubscribeCustomMessagesRequest>,
    ) -> Result<Response<Self::SubscribeCustomMessagesStream>, Status> {
        todo!()
    }

    async fn list_aliases(
        &self,
        request: Request<ListAliasesRequest>,
    ) -> Result<Response<ListAliasesResponse>, Status> {
        todo!()
    }

    async fn lookup_htlc_resolution(
        &self,
        request: Request<LookupHtlcResolutionRequest>,
    ) -> Result<Response<LookupHtlcResolutionResponse>, Status> {
        todo!()
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
        fallback_addr: "".to_string(),
        cltv_expiry: bolt11.min_final_cltv_expiry_delta(),
        route_hints,
        private: !bolt11.route_hints().is_empty(),
        add_index: 0,
        settle_index: 0,
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
