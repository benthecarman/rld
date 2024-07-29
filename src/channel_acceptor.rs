use bitcoin::secp256k1::PublicKey;
use bitcoin::Address;
use lightning::ln::ChannelId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

type Sender = mpsc::Sender<ChannelAcceptorResponse>;
type Receiver = mpsc::Receiver<ChannelAcceptorResponse>;
type Listeners = Arc<Mutex<HashMap<ChannelId, Sender>>>;

pub(crate) struct ChannelAcceptor {
    sender: broadcast::Sender<ChannelAcceptorRequest>,
    listeners: Listeners,
}

impl ChannelAcceptor {
    pub(crate) fn new(sender: broadcast::Sender<ChannelAcceptorRequest>) -> Self {
        Self {
            sender,
            listeners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn send_request(&self, response: ChannelAcceptorRequest) -> anyhow::Result<usize> {
        Ok(self.sender.send(response)?)
    }

    pub(crate) async fn send_response(&self, response: ChannelAcceptorResponse) {
        let mut listeners = self.listeners.lock().await;
        if let Some(sender) = listeners.get_mut(&response.pending_chan_id) {
            sender
                .send(response)
                .await
                .expect("Failed to send response");
        }
    }

    pub(crate) async fn add_listener(&self, id: ChannelId) -> Receiver {
        let (sender, receiver) = mpsc::channel(100);
        let mut listeners = self.listeners.lock().await;
        listeners.insert(id, sender);
        receiver
    }
}

#[derive(Debug, Clone)]
pub struct ChannelAcceptorResponse {
    /// Whether or not the client accepts the channel.
    pub accept: bool,
    /// The pending channel id to which this response applies.
    pub pending_chan_id: ChannelId,
    /// An optional error to send the initiating party to indicate why the channel
    /// was rejected. This field *should not* contain sensitive information, it will
    /// be sent to the initiating party. This field should only be set if accept is
    /// false, the channel will be rejected if an error is set with accept=true
    /// because the meaning of this response is ambiguous. Limited to 500
    /// characters.
    pub error: String,
    /// The upfront shutdown address to use if the initiating peer supports option
    /// upfront shutdown script (see ListPeers for the features supported). Note
    /// that the channel open will fail if this value is set for a peer that does
    /// not support this feature bit.
    pub upfront_shutdown: Option<Address>,
    /// The csv delay (in blocks) that we require for the remote party.
    pub csv_delay: u32,
    /// We require that the remote peer always have some reserve amount allocated to
    /// them so that there is always a disincentive to broadcast old state (if they
    /// hold 0 sats on their side of the channel, there is nothing to lose).
    pub reserve_sat: u64,
    /// The maximum amount of funds in millisatoshis that we allow the remote peer
    /// to have in outstanding htlcs.
    pub in_flight_max_msat: u64,
    /// The maximum number of htlcs that the remote peer can offer us.
    pub max_htlc_count: u32,
    /// The minimum value in millisatoshis for incoming htlcs on the channel.
    pub min_htlc_in: u64,
    /// The number of confirmations we require before we consider the channel open.
    pub min_accept_depth: u32,
    /// Whether the responder wants this to be a zero-conf channel. This will fail
    /// if either side does not have the scid-alias feature bit set. The minimum
    /// depth field must be zero if this is true.
    pub zero_conf: bool,
}

#[derive(Debug, Clone)]
pub struct ChannelAcceptorRequest {
    /// The pubkey of the node that wishes to open an inbound channel.
    pub node_pubkey: PublicKey,
    /// The hash of the genesis block that the proposed channel resides in.
    pub chain_hash: Vec<u8>,
    /// The pending channel id.
    pub pending_chan_id: ChannelId,
    /// The funding amount in satoshis that initiator wishes to use in the
    /// channel.
    pub funding_amt: u64,
    /// The push amount of the proposed channel in millisatoshis.
    pub push_amt: u64,
    /// The dust limit of the initiator's commitment tx.
    pub dust_limit: u64,
    /// The maximum amount of coins in millisatoshis that can be pending in this
    /// channel.
    pub max_value_in_flight: u64,
    /// The minimum amount of satoshis the initiator requires us to have at all
    /// times.
    pub channel_reserve: u64,
    /// The smallest HTLC in millisatoshis that the initiator will accept.
    pub min_htlc: u64,
    /// The initial fee rate that the initiator suggests for both commitment
    /// transactions.
    pub fee_per_kw: u64,
    /// The number of blocks to use for the relative time lock in the pay-to-self
    /// output of both commitment transactions.
    pub csv_delay: u32,
    /// The total number of incoming HTLC's that the initiator will accept.
    pub max_accepted_htlcs: u32,
    /// A bit-field which the initiator uses to specify proposed channel
    /// behavior.
    pub channel_flags: u32,
    /// The commitment type the initiator wishes to use for the proposed channel.
    pub commitment_type: i32,
    /// Whether the initiator wants to open a zero-conf channel via the channel
    /// type.
    pub wants_zero_conf: bool,
    /// Whether the initiator wants to use the scid-alias channel type. This is
    /// separate from the feature bit.
    pub wants_scid_alias: bool,
}
