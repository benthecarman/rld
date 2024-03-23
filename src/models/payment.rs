use super::schema::payments;
use bitcoin::secp256k1::PublicKey;
use diesel::prelude::*;
use lightning::offers::offer::Offer;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(
    Queryable,
    Insertable,
    Identifiable,
    AsChangeset,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
)]
#[diesel(primary_key(payment_hash))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Payment {
    payment_hash: Vec<u8>,
    preimage: Option<Vec<u8>>,
    pub amount_msats: i32,
    pub fee_msats: Option<i32>,
    destination_pubkey: Option<Vec<u8>>,
    bolt11: Option<String>,
    bolt12: Option<String>,
    pub status: i16,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = payments)]
pub struct NewPayment {
    pub payment_hash: Vec<u8>,
    pub amount_msats: i32,
    pub fee_msats: Option<i32>,
    pub destination_pubkey: Option<Vec<u8>>,
    pub bolt11: Option<String>,
    pub bolt12: Option<String>,
    pub status: i16,
}

impl Payment {
    pub fn payment_hash(&self) -> [u8; 32] {
        self.payment_hash
            .as_slice()
            .try_into()
            .expect("invalid payment hash")
    }

    pub fn preimage(&self) -> Option<[u8; 32]> {
        self.preimage
            .as_ref()
            .map(|p| p.as_slice().try_into().expect("invalid preimage"))
    }

    pub fn amount_msats(&self) -> i32 {
        self.amount_msats
    }

    pub fn fee_msats(&self) -> Option<i32> {
        self.fee_msats
    }

    // todo might be better to have as a NodeId
    pub fn destination_pubkey(&self) -> Option<PublicKey> {
        self.destination_pubkey
            .as_ref()
            .map(|d| PublicKey::from_slice(d).expect("invalid pubkey"))
    }

    pub fn bolt11(&self) -> Option<Bolt11Invoice> {
        self.bolt11
            .as_ref()
            .map(|b| Bolt11Invoice::from_str(b).expect("invalid bolt11"))
    }

    pub fn bolt12(&self) -> Option<Offer> {
        self.bolt12
            .as_ref()
            .map(|b| Offer::from_str(b).expect("invalid bolt12"))
    }

    pub fn status(&self) -> i16 {
        self.status
    }
}
