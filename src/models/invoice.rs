use crate::models::schema::invoices;
use diesel::prelude::*;
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
pub struct Invoice {
    payment_hash: Vec<u8>,
    preimage: Option<Vec<u8>>,
    bolt11: String,
    pub amount_msats: Option<i32>,
    pub status: i16,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = invoices)]
pub struct NewInvoice {
    pub payment_hash: Vec<u8>,
    pub preimage: Option<Vec<u8>>,
    pub amount_msats: Option<i32>,
    pub bolt11: String,
    pub status: i16,
}

impl Invoice {
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

    pub fn bolt11(&self) -> Bolt11Invoice {
        Bolt11Invoice::from_str(&self.bolt11).expect("invalid bolt11")
    }

    pub fn status(&self) -> i16 {
        self.status
    }

    pub fn find_by_payment_hash(
        conn: &mut PgConnection,
        payment_hash: &[u8],
    ) -> anyhow::Result<Option<Invoice>> {
        Ok(invoices::table
            .filter(invoices::payment_hash.eq(payment_hash))
            .first(conn)
            .optional()?)
    }
}
