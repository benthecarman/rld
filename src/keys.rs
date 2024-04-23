use crate::logger::RldLogger;
use crate::onchain::OnChainWallet;
use bdk::wallet::AddressIndex;
use bitcoin::absolute::LockTime;
use bitcoin::bech32::u5;
use bitcoin::bip32::{DerivationPath, ExtendedPrivKey};
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{Network, ScriptBuf, Transaction, TxOut};
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::offers::invoice::UnsignedBolt12Invoice;
use lightning::offers::invoice_request::UnsignedInvoiceRequest;
use lightning::sign::{
    EntropySource, InMemorySigner, KeyMaterial, KeysManager as LdkKeysManager, NodeSigner,
    OutputSpender, Recipient, SignerProvider, SpendableOutputDescriptor,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

pub struct KeysManager {
    inner: LdkKeysManager,
    wallet: Arc<OnChainWallet>,
    logger: Arc<RldLogger>,
}

impl KeysManager {
    pub fn new(
        xprv: ExtendedPrivKey,
        network: Network,
        wallet: Arc<OnChainWallet>,
        logger: Arc<RldLogger>,
    ) -> anyhow::Result<Self> {
        let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
        let path =
            DerivationPath::from_str(&format!("m/1038h/{}h/0h", coin_type_from_network(network)))?;
        let seed = xprv.derive_priv(&Secp256k1::new(), &path)?.private_key;

        let inner = LdkKeysManager::new(seed.as_ref(), cur.as_secs(), cur.subsec_nanos());
        Ok(Self {
            inner,
            wallet,
            logger,
        })
    }

    pub(crate) fn get_node_secret_key(&self) -> SecretKey {
        self.inner.get_node_secret_key()
    }

    /// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
    pub fn spend_spendable_outputs<C: Signing>(
        &self,
        descriptors: &[&SpendableOutputDescriptor],
        outputs: Vec<TxOut>,
        feerate_sat_per_1000_weight: u32,
        locktime: Option<LockTime>,
        secp_ctx: &Secp256k1<C>,
    ) -> Result<Transaction, ()> {
        let address = {
            let mut wallet = self.wallet.wallet.try_write().map_err(|_| ())?;
            // These often fail because we continually retry these. Use LastUnused so we don't generate a ton of new
            // addresses for no reason.
            wallet
                .try_get_internal_address(AddressIndex::LastUnused)
                .map_err(|_| ())?
                .address
        };

        self.inner.spend_spendable_outputs(
            descriptors,
            outputs,
            address.script_pubkey(),
            feerate_sat_per_1000_weight,
            locktime,
            secp_ctx,
        )
    }
}

impl EntropySource for KeysManager {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        self.inner.get_secure_random_bytes()
    }
}

impl NodeSigner for KeysManager {
    fn get_inbound_payment_key_material(&self) -> KeyMaterial {
        self.inner.get_inbound_payment_key_material()
    }

    fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
        self.inner.get_node_id(recipient)
    }

    fn ecdh(
        &self,
        recipient: Recipient,
        other_key: &PublicKey,
        tweak: Option<&Scalar>,
    ) -> Result<SharedSecret, ()> {
        self.inner.ecdh(recipient, other_key, tweak)
    }

    fn sign_invoice(
        &self,
        hrp_bytes: &[u8],
        invoice_data: &[u5],
        recipient: Recipient,
    ) -> Result<RecoverableSignature, ()> {
        self.inner.sign_invoice(hrp_bytes, invoice_data, recipient)
    }

    fn sign_bolt12_invoice_request(
        &self,
        invoice_request: &UnsignedInvoiceRequest,
    ) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
        self.inner.sign_bolt12_invoice_request(invoice_request)
    }

    fn sign_bolt12_invoice(
        &self,
        invoice: &UnsignedBolt12Invoice,
    ) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
        self.inner.sign_bolt12_invoice(invoice)
    }

    fn sign_gossip_message(&self, msg: UnsignedGossipMessage) -> Result<Signature, ()> {
        self.inner.sign_gossip_message(msg)
    }
}

impl SignerProvider for KeysManager {
    type EcdsaSigner = InMemorySigner;

    fn generate_channel_keys_id(
        &self,
        inbound: bool,
        channel_value_satoshis: u64,
        user_channel_id: u128,
    ) -> [u8; 32] {
        self.inner
            .generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
    }

    fn derive_channel_signer(
        &self,
        channel_value_satoshis: u64,
        channel_keys_id: [u8; 32],
    ) -> Self::EcdsaSigner {
        self.inner
            .derive_channel_signer(channel_value_satoshis, channel_keys_id)
    }

    fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::EcdsaSigner, DecodeError> {
        self.inner.read_chan_signer(reader)
    }

    fn get_destination_script(&self, _channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
        let mut wallet = self.wallet.wallet.try_write().map_err(|_| ())?;
        Ok(wallet
            .try_get_address(AddressIndex::New)
            .map_err(|_| ())?
            .address
            .script_pubkey())
    }

    fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
        let mut wallet = self.wallet.wallet.try_write().map_err(|_| ())?;
        let script = wallet
            .try_get_address(AddressIndex::New)
            .map_err(|_| ())?
            .address
            .script_pubkey();
        ShutdownScript::try_from(script).map_err(|_| ())
    }
}

pub(crate) fn coin_type_from_network(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 0,
        Network::Testnet => 1,
        Network::Signet => 1,
        Network::Regtest => 1,
        net => panic!("Got unknown network: {net}!"),
    }
}
