use super::schema::payments;
use bitcoin::secp256k1::PublicKey;
use diesel::prelude::*;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::PaymentHash;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::routing::router::{BlindedTail, Path, RouteHop};
use lightning::util::ser::{Readable, Writeable};
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum PaymentStatus {
    Pending = 1,
    Completed = 2,
    Failed = 3,
}

impl PaymentStatus {
    pub fn from_i16(status: i16) -> Option<Self> {
        match status {
            1 => Some(PaymentStatus::Pending),
            2 => Some(PaymentStatus::Completed),
            3 => Some(PaymentStatus::Failed),
            _ => None,
        }
    }
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
pub struct Payment {
    pub id: i32,
    pub(crate) payment_id: Vec<u8>,
    pub(crate) payment_hash: Vec<u8>,
    preimage: Option<Vec<u8>>,
    pub amount_msats: i64,
    pub fee_msats: Option<i64>,
    destination_pubkey: Option<Vec<u8>>,
    bolt11: Option<String>,
    bolt12: Option<Vec<u8>>,
    status: i16,
    path: Option<Vec<u8>>,
    blinded_tail: Option<Vec<u8>>,
    pub created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = payments)]
struct NewPayment {
    payment_id: Vec<u8>,
    payment_hash: Vec<u8>,
    amount_msats: i64,
    destination_pubkey: Option<Vec<u8>>,
    bolt11: Option<String>,
    bolt12: Option<Vec<u8>>,
    status: i16,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = payments)]
struct CompletedPayment {
    payment_hash: Vec<u8>,
    preimage: Vec<u8>,
    fee_msats: i64,
    status: i16,
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

    pub fn amount_msats(&self) -> i64 {
        self.amount_msats
    }

    pub fn fee_msats(&self) -> Option<i64> {
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

    pub fn bolt12(&self) -> Option<Bolt12Invoice> {
        self.bolt12
            .clone()
            .map(|b| Bolt12Invoice::try_from(b).expect("invalid bolt12"))
    }

    pub fn status(&self) -> PaymentStatus {
        PaymentStatus::from_i16(self.status).expect("invalid payment status")
    }

    pub fn path(&self) -> Option<Vec<RouteHop>> {
        self.path.as_ref().map(|p| {
            let mut cursor = std::io::Cursor::new(p);
            let mut hops = Vec::new();
            while cursor.position() < p.len() as u64 {
                hops.push(RouteHop::read(&mut cursor).expect("invalid route hop"));
            }
            hops
        })
    }

    pub fn blinded_tail(&self) -> Option<BlindedTail> {
        self.blinded_tail.as_ref().map(|p| {
            let mut cursor = std::io::Cursor::new(p);
            BlindedTail::read(&mut cursor).expect("invalid blinded tail")
        })
    }

    pub fn find_by_payment_id(
        conn: &mut PgConnection,
        payment_id: PaymentId,
    ) -> anyhow::Result<Option<Payment>> {
        Ok(payments::table
            .filter(payments::payment_id.eq(payment_id.0.as_slice()))
            .get_result(conn)
            .optional()?)
    }

    pub fn find_by_payment_hash(
        conn: &mut PgConnection,
        payment_hash: [u8; 32],
    ) -> anyhow::Result<Option<Payment>> {
        Ok(payments::table
            .filter(payments::payment_hash.eq(payment_hash.as_slice()))
            .get_result(conn)
            .optional()?)
    }

    pub fn create(
        conn: &mut PgConnection,
        payment_id: PaymentId,
        payment_hash: PaymentHash,
        amount_msats: i64,
        destination_pubkey: Option<PublicKey>,
        bolt11: Option<Bolt11Invoice>,
        bolt12: Option<&Bolt12Invoice>,
    ) -> anyhow::Result<Payment> {
        let new = NewPayment {
            payment_id: payment_id.0.to_vec(),
            payment_hash: payment_hash.0.to_vec(),
            amount_msats,
            destination_pubkey: destination_pubkey.map(|d| d.serialize().to_vec()),
            bolt11: bolt11.map(|b| b.to_string()),
            bolt12: bolt12.map(|b| b.encode()),
            status: PaymentStatus::Pending as i16,
        };

        Ok(diesel::insert_into(payments::table)
            .values(&new)
            .get_result(conn)?)
    }

    pub fn payment_complete(
        conn: &mut PgConnection,
        payment_hash: PaymentHash,
        preimage: [u8; 32],
        fee_msats: i64,
    ) -> anyhow::Result<Payment> {
        let completed = CompletedPayment {
            payment_hash: payment_hash.0.to_vec(),
            preimage: preimage.to_vec(),
            fee_msats,
            status: PaymentStatus::Completed as i16,
        };

        let updated_payment = diesel::update(
            payments::table.filter(payments::payment_hash.eq(payment_hash.0.as_slice())),
        )
        .set(&completed)
        .get_result(conn)?;

        Ok(updated_payment)
    }

    pub fn add_path(
        conn: &mut PgConnection,
        payment_hash: PaymentHash,
        path: Path,
    ) -> anyhow::Result<Payment> {
        let mut hops_bytes = Vec::new();
        for hop in path.hops {
            hop.write(&mut hops_bytes)?;
        }
        let blinded_tail = path.blinded_tail.map(|t| t.encode());

        let res = diesel::update(
            payments::table.filter(payments::payment_hash.eq(payment_hash.0.as_slice())),
        )
        .set((
            payments::path.eq(hops_bytes),
            payments::blinded_tail.eq(blinded_tail),
        ))
        .get_result(conn)?;

        Ok(res)
    }

    pub fn payment_failed(
        conn: &mut PgConnection,
        payment_hash: PaymentHash,
    ) -> anyhow::Result<Payment> {
        let res = diesel::update(
            payments::table.filter(payments::payment_hash.eq(payment_hash.0.as_slice())),
        )
        .set(payments::status.eq(PaymentStatus::Failed as i16))
        .get_result(conn)?;

        Ok(res)
    }

    pub fn list_payments(conn: &mut PgConnection) -> anyhow::Result<Vec<Payment>> {
        Ok(payments::table.load(conn)?)
    }
}
