use crate::models::schema::channel_open_params;
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
pub struct ChannelOpenParam {
    pub id: i32,
    sats_per_vbyte: Option<i32>,
    opening_tx: Option<Vec<u8>>,
    pub success: bool,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = channel_open_params)]
pub struct NewChannelOpenParam {
    pub sats_per_vbyte: Option<i32>,
}

impl ChannelOpenParam {
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

    pub fn create(
        conn: &mut PgConnection,
        sats_per_vbyte: Option<i32>,
    ) -> anyhow::Result<ChannelOpenParam> {
        let new = NewChannelOpenParam { sats_per_vbyte };

        Ok(diesel::insert_into(channel_open_params::table)
            .values(&new)
            .get_result(conn)?)
    }

    pub fn save(&self, conn: &mut PgConnection) -> anyhow::Result<()> {
        diesel::update(channel_open_params::table.find(self.id))
            .set(self)
            .execute(conn)?;
        Ok(())
    }

    pub fn find_by_id(
        conn: &mut PgConnection,
        id: i32,
    ) -> anyhow::Result<Option<ChannelOpenParam>> {
        Ok(channel_open_params::table
            .find(id)
            .first::<ChannelOpenParam>(conn)
            .optional()?)
    }

    pub fn mark_success(conn: &mut PgConnection, id: i32) -> anyhow::Result<()> {
        diesel::update(channel_open_params::table.find(id))
            .set(channel_open_params::success.eq(true))
            .execute(conn)?;
        Ok(())
    }
}
