use crate::models::schema::connection_info;
use diesel::{
    AsChangeset, Connection, ExpressionMethods, Identifiable, Insertable, PgConnection, QueryDsl,
    Queryable, RunQueryDsl,
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
#[diesel(primary_key(node_id, connection_string))]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(table_name = connection_info)]
pub struct ConnectionInfo {
    pub node_id: Vec<u8>,
    pub connection_string: String,
    pub reconnect: bool,
    updated_at: chrono::NaiveDateTime,
}

#[derive(Clone, Insertable, AsChangeset)]
#[diesel(table_name = connection_info)]
pub struct NewConnectionInfo {
    pub node_id: Vec<u8>,
    pub connection_string: String,
    pub reconnect: bool,
}

impl ConnectionInfo {
    pub fn upsert(
        conn: &mut PgConnection,
        node_id: Vec<u8>,
        connection_string: String,
        reconnect: bool,
    ) -> anyhow::Result<()> {
        let new = NewConnectionInfo {
            node_id: node_id.clone(),
            connection_string,
            reconnect,
        };

        diesel::insert_into(connection_info::table)
            .values(new.clone())
            .get_result::<Self>(conn)
            .map(|_| ())
            .or_else(|_| {
                diesel::update(connection_info::table.filter(connection_info::node_id.eq(node_id)))
                    .set(new)
                    .get_result::<Self>(conn)
                    .map(|_| ())
            })?;

        Ok(())
    }

    pub fn set_reconnect(conn: &mut PgConnection, node_id: Vec<u8>) -> anyhow::Result<()> {
        // find the most recently updated connection info and set the reconnect flag
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            let mut info = connection_info::table
                .filter(connection_info::node_id.eq(&node_id))
                .order_by(connection_info::reconnect.desc())
                .order_by(connection_info::updated_at.desc())
                .first::<Self>(conn)?;

            // update the connection info
            info.reconnect = true;
            diesel::update(
                connection_info::table
                    .filter(connection_info::node_id.eq(&node_id))
                    .filter(connection_info::connection_string.eq(info.connection_string.clone())),
            )
            .set(info)
            .execute(conn)?;

            Ok(())
        })?;

        Ok(())
    }
    pub fn find_by_node_id(
        conn: &mut PgConnection,
        node_id: Vec<u8>,
    ) -> anyhow::Result<Vec<String>> {
        Ok(connection_info::table
            .filter(connection_info::node_id.eq(node_id))
            .select(connection_info::connection_string)
            .order_by(connection_info::reconnect.desc())
            .order_by(connection_info::updated_at.desc())
            .get_results(conn)?)
    }
}
