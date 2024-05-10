use crate::models::schema::channels;
use bitcoin::consensus::Decodable;
use bitcoin::{FeeRate, Transaction};
use diesel::prelude::*;
use lightning::util::ser::Writeable;
use serde::{Deserialize, Serialize};

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
pub struct Channel {
    pub id: i32,
    node_id: Vec<u8>,
    sats_per_vbyte: Option<i32>,
    push_amount_sat: i64,
    private: bool,
    initiator: bool,
    capacity: i64,
    zero_conf: bool,
    funding_txo: Option<String>,
    channel_id: Option<Vec<u8>>,
    opening_tx: Option<Vec<u8>>,
    pub success: bool,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = channels)]
pub struct NewChannel {
    node_id: Vec<u8>,
    sats_per_vbyte: Option<i32>,
    push_amount_sat: i64,
    private: bool,
    initiator: bool,
    capacity: i64,
    zero_conf: bool,
}

impl Channel {
    pub fn sats_per_vbyte(&self) -> Option<FeeRate> {
        self.sats_per_vbyte
            .map(|s| FeeRate::from_sat_per_vb(s as u64).expect("invalid sats_per_vbyte"))
    }

    pub fn opening_tx(&self) -> Option<Transaction> {
        self.opening_tx.as_ref().map(|tx| {
            let mut cursor = std::io::Cursor::new(tx);
            Transaction::consensus_decode(&mut cursor).expect("invalid opening tx")
        })
    }

    pub fn set_opening_tx(&mut self, tx: &Transaction) {
        self.opening_tx = Some(tx.encode());
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        conn: &mut PgConnection,
        node_id: Vec<u8>,
        sats_per_vbyte: Option<i32>,
        push_amount_sat: i64,
        private: bool,
        initiator: bool,
        capacity: i64,
        zero_conf: bool,
    ) -> anyhow::Result<Channel> {
        let new = NewChannel {
            node_id,
            sats_per_vbyte,
            push_amount_sat,
            private,
            initiator,
            capacity,
            zero_conf,
        };

        Ok(diesel::insert_into(channels::table)
            .values(&new)
            .get_result(conn)?)
    }

    pub fn save(&self, conn: &mut PgConnection) -> anyhow::Result<()> {
        diesel::update(channels::table.find(self.id))
            .set(self)
            .execute(conn)?;
        Ok(())
    }

    pub fn find_by_id(conn: &mut PgConnection, id: i32) -> anyhow::Result<Option<Channel>> {
        Ok(channels::table.find(id).first::<Channel>(conn).optional()?)
    }

    pub fn mark_success(
        conn: &mut PgConnection,
        id: i32,
        funding_txo: String,
        channel_id: Vec<u8>,
    ) -> anyhow::Result<()> {
        diesel::update(channels::table.find(id))
            .set((
                channels::success.eq(true),
                channels::funding_txo.eq(Some(funding_txo)),
                channels::channel_id.eq(Some(channel_id)),
            ))
            .execute(conn)?;
        Ok(())
    }
}
