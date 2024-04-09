use crate::models::schema::invoices;
use bitcoin::hashes::Hash;
use diesel::prelude::*;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum InvoiceStatus {
    Pending = 0,
    Paid = 1,
    Expired = 2,
    Held = 3,
}

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
#[diesel(primary_key(id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Invoice {
    pub id: i32,
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

    pub fn status(&self) -> InvoiceStatus {
        match self.status {
            0 => InvoiceStatus::Pending,
            1 => InvoiceStatus::Paid,
            2 => InvoiceStatus::Expired,
            3 => InvoiceStatus::Held,
            _ => panic!("Invalid invoice status")
        }
    }

    pub fn creation_date(&self) -> i64 {
        self.created_at.and_utc().timestamp()
    }

    pub fn updated_date(&self) -> i64 {
        self.updated_at.and_utc().timestamp()
    }

    pub fn create(conn: &mut PgConnection, invoice: &Bolt11Invoice) -> anyhow::Result<Invoice> {
        let new_invoice = NewInvoice {
            payment_hash: invoice.payment_hash().as_byte_array().to_vec(),
            preimage: None,
            amount_msats: invoice.amount_milli_satoshis().map(|a| a as i32),
            bolt11: invoice.to_string(),
            status: InvoiceStatus::Pending as i16,
        };

        Ok(diesel::insert_into(invoices::table)
            .values(&new_invoice)
            .get_result(conn)?)
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

    pub fn mark_as_paid(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
        preimage: Option<[u8; 32]>,
        amount_msats: i32,
    ) -> anyhow::Result<Self> {
        let res = match preimage {
            Some(preimage) => diesel::update(invoices::table)
                .filter(invoices::payment_hash.eq(payment_hash.as_slice()))
                .set((
                    invoices::preimage.eq(preimage.as_slice()),
                    invoices::status.eq(InvoiceStatus::Paid as i16),
                    invoices::amount_msats.eq(Some(amount_msats)),
                ))
                .get_result(conn)?,
            None => diesel::update(invoices::table)
                .filter(invoices::payment_hash.eq(payment_hash.as_slice()))
                .set((
                    invoices::status.eq(InvoiceStatus::Paid as i16),
                    invoices::amount_msats.eq(Some(amount_msats)),
                ))
                .get_result(conn)?,
        };

        Ok(res)
    }
}
