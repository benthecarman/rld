use crate::models::schema::receives;
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
    Canceled = 4,
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
pub struct Receive {
    pub id: i32,
    payment_hash: Vec<u8>,
    preimage: Option<Vec<u8>>,
    bolt11: Option<String>,
    pub amount_msats: Option<i64>,
    pub status: i16,
    pub created_at: chrono::NaiveDateTime,
    pub settled_at: Option<chrono::NaiveDateTime>,
    pub updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = receives)]
pub struct NewReceive {
    pub payment_hash: Vec<u8>,
    pub preimage: Option<Vec<u8>>,
    pub amount_msats: Option<i64>,
    pub bolt11: Option<String>,
    pub status: i16,
}

impl Receive {
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

    pub fn bolt11(&self) -> Option<Bolt11Invoice> {
        self.bolt11
            .as_deref()
            .map(|b| Bolt11Invoice::from_str(b).expect("invalid bolt11"))
    }

    pub fn status(&self) -> InvoiceStatus {
        match self.status {
            0 => InvoiceStatus::Pending,
            1 => InvoiceStatus::Paid,
            2 => InvoiceStatus::Expired,
            3 => InvoiceStatus::Held,
            4 => InvoiceStatus::Canceled,
            _ => panic!("Invalid invoice status"),
        }
    }

    pub fn creation_date(&self) -> i64 {
        self.created_at.and_utc().timestamp()
    }

    pub fn settled_at(&self) -> Option<i64> {
        self.settled_at.map(|s| s.and_utc().timestamp())
    }

    pub fn updated_date(&self) -> i64 {
        self.updated_at.and_utc().timestamp()
    }

    pub fn create(conn: &mut PgConnection, invoice: &Bolt11Invoice) -> anyhow::Result<Receive> {
        let new_invoice = NewReceive {
            payment_hash: invoice.payment_hash().as_byte_array().to_vec(),
            preimage: None,
            amount_msats: invoice.amount_milli_satoshis().map(|a| a as i64),
            bolt11: Some(invoice.to_string()),
            status: InvoiceStatus::Pending as i16,
        };

        Ok(diesel::insert_into(receives::table)
            .values(&new_invoice)
            .get_result(conn)?)
    }

    pub fn create_keysend(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
        amount_msats: i64,
    ) -> anyhow::Result<Receive> {
        let keysend = NewReceive {
            payment_hash: payment_hash.to_vec(),
            preimage: Some(preimage.to_vec()),
            amount_msats: Some(amount_msats),
            bolt11: None,
            status: InvoiceStatus::Paid as i16,
        };

        Ok(diesel::insert_into(receives::table)
            .values(&keysend)
            .get_result(conn)?)
    }

    pub fn find_by_payment_hash(
        conn: &mut PgConnection,
        payment_hash: &[u8],
    ) -> anyhow::Result<Option<Receive>> {
        Ok(receives::table
            .filter(receives::payment_hash.eq(payment_hash))
            .first(conn)
            .optional()?)
    }

    pub fn mark_as_paid(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
        preimage: Option<[u8; 32]>,
        amount_msats: i64,
    ) -> anyhow::Result<Self> {
        let res = match preimage {
            Some(preimage) => diesel::update(receives::table)
                .filter(receives::payment_hash.eq(payment_hash.as_slice()))
                .set((
                    receives::preimage.eq(preimage.as_slice()),
                    receives::status.eq(InvoiceStatus::Paid as i16),
                    receives::amount_msats.eq(Some(amount_msats)),
                    receives::settled_at.eq(Some(chrono::Utc::now().naive_utc())),
                ))
                .get_result(conn)?,
            None => diesel::update(receives::table)
                .filter(receives::payment_hash.eq(payment_hash.as_slice()))
                .set((
                    receives::status.eq(InvoiceStatus::Paid as i16),
                    receives::amount_msats.eq(Some(amount_msats)),
                    receives::settled_at.eq(Some(chrono::Utc::now().naive_utc())),
                ))
                .get_result(conn)?,
        };

        Ok(res)
    }

    pub fn mark_as_expired(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
    ) -> anyhow::Result<Self> {
        let res = diesel::update(receives::table)
            .filter(receives::payment_hash.eq(payment_hash.as_slice()))
            .set(receives::status.eq(InvoiceStatus::Expired as i16))
            .get_result(conn)?;

        Ok(res)
    }

    pub fn mark_as_canceled(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
    ) -> anyhow::Result<Option<Self>> {
        let res = diesel::update(receives::table)
            .filter(receives::payment_hash.eq(payment_hash.as_slice()))
            // only update if the invoice is pending or held
            .filter(
                receives::status
                    .eq(InvoiceStatus::Pending as i16)
                    .or(receives::status.eq(InvoiceStatus::Held as i16)),
            )
            .set(receives::status.eq(InvoiceStatus::Canceled as i16))
            .get_result(conn)
            .optional()?;
        Ok(res)
    }

    pub fn mark_as_held(conn: &mut PgConnection, payment_hash: [u8; 32]) -> anyhow::Result<Self> {
        let res = diesel::update(receives::table)
            .filter(receives::payment_hash.eq(payment_hash.as_slice()))
            .set(receives::status.eq(InvoiceStatus::Held as i16))
            .get_result(conn)?;

        Ok(res)
    }

    pub fn find_all(conn: &mut PgConnection) -> anyhow::Result<Vec<Receive>> {
        Ok(receives::table.load(conn)?)
    }
}
