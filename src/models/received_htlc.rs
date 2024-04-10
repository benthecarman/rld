use crate::models::schema::received_htlcs;
use diesel::{
    AsChangeset, ExpressionMethods, Identifiable, Insertable, PgConnection, QueryDsl, Queryable,
    RunQueryDsl,
};
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
pub struct ReceivedHtlc {
    pub id: i32,
    pub receive_id: i32,
    pub amount_msats: i64,
    pub channel_id: i32,
    pub cltv_expiry: i64,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = received_htlcs)]
pub struct NewReceivedHtlc {
    pub receive_id: i32,
    pub amount_msats: i64,
    pub channel_id: i32,
    pub cltv_expiry: i64,
}

impl ReceivedHtlc {
    pub fn create(
        conn: &mut PgConnection,
        receive_id: i32,
        amount_msats: i64,
        channel_id: i32,
        cltv_expiry: i64,
    ) -> anyhow::Result<Self> {
        let htlc = NewReceivedHtlc {
            receive_id,
            amount_msats,
            channel_id,
            cltv_expiry,
        };

        Ok(diesel::insert_into(received_htlcs::table)
            .values(htlc)
            .get_result(conn)?)
    }

    pub fn find_by_id(conn: &mut PgConnection, id: i32) -> anyhow::Result<Self> {
        Ok(received_htlcs::table.find(id).first(conn)?)
    }

    pub fn find_by_receive_id(
        conn: &mut PgConnection,
        receive_id: i32,
    ) -> anyhow::Result<Vec<Self>> {
        Ok(received_htlcs::table
            .filter(received_htlcs::receive_id.eq(receive_id))
            .load(conn)?)
    }
}
