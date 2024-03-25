use std::str::FromStr;

use bitcoin::secp256k1::PublicKey;
use bitcoin::OutPoint;
use diesel::prelude::*;
use lightning::util::ser::Writeable;
use serde::{Deserialize, Serialize};

use crate::models::schema::channel_closures;

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
pub struct ChannelClosure {
    pub id: i32,
    node_id: Vec<u8>,
    funding_txo: Option<String>,
    reason: String,
    created_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = channel_closures)]
pub struct NewChannelClosure {
    id: i32,
    node_id: Vec<u8>,
    funding_txo: Option<String>,
    reason: String,
}

impl ChannelClosure {
    pub fn node_id(&self) -> PublicKey {
        PublicKey::from_slice(&self.node_id).expect("invalid node_id")
    }

    pub fn funding_txo(&self) -> Option<OutPoint> {
        self.funding_txo
            .as_ref()
            .map(|str| OutPoint::from_str(str).expect("invalid funding_txo"))
    }

    pub fn create(
        conn: &mut PgConnection,
        id: i32,
        node_id: PublicKey,
        funding_txo: Option<OutPoint>,
        reason: String,
    ) -> anyhow::Result<ChannelClosure> {
        let new = NewChannelClosure {
            id,
            node_id: node_id.encode(),
            funding_txo: funding_txo.map(|x| x.to_string()),
            reason,
        };

        Ok(diesel::insert_into(channel_closures::table)
            .values(&new)
            .get_result(conn)?)
    }

    pub fn find_by_id(conn: &mut PgConnection, id: i32) -> anyhow::Result<Option<ChannelClosure>> {
        Ok(channel_closures::table.find(id).first(conn).optional()?)
    }
}
