use crate::models::schema::routed_payments;
use diesel::{
    AsChangeset, Connection, ExpressionMethods, Identifiable, Insertable, OptionalExtension,
    PgConnection, QueryDsl, Queryable, RunQueryDsl,
};
use serde::{Deserialize, Serialize};
use std::ops::Sub;

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
pub struct RoutedPayment {
    pub id: i32,
    pub prev_channel_id: Vec<u8>,
    pub prev_scid: i64,
    pub next_channel_id: Vec<u8>,
    pub next_scid: i64,
    pub fee_earned_msat: i64,
    pub amount_forwarded: i64,
    pub(crate) created_at: chrono::NaiveDateTime,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = routed_payments)]
pub struct NewRoutedPayment {
    pub prev_channel_id: Vec<u8>,
    pub prev_scid: i64,
    pub next_channel_id: Vec<u8>,
    pub next_scid: i64,
    pub fee_earned_msat: i64,
    pub amount_forwarded: i64,
}

impl RoutedPayment {
    pub fn create(
        conn: &mut PgConnection,
        prev_channel_id: Vec<u8>,
        prev_scid: i64,
        next_channel_id: Vec<u8>,
        next_scid: i64,
        fee_earned_msat: i64,
        amount_forwarded: i64,
    ) -> anyhow::Result<Self> {
        let new = NewRoutedPayment {
            prev_channel_id,
            prev_scid,
            next_channel_id,
            next_scid,
            fee_earned_msat,
            amount_forwarded,
        };

        Ok(diesel::insert_into(routed_payments::table)
            .values(new)
            .get_result(conn)?)
    }

    pub fn find_by_id(conn: &mut PgConnection, id: i32) -> anyhow::Result<Option<Self>> {
        Ok(routed_payments::table.find(id).first(conn).optional()?)
    }

    pub fn get_fee_report(conn: &mut PgConnection) -> anyhow::Result<FeeReport> {
        // todo figure how to get SQL sums working
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            let now = chrono::Utc::now().naive_utc();
            let daily_fee_earned_msat = routed_payments::table
                .filter(
                    routed_payments::created_at.gt(now.sub(chrono::Duration::try_days(1).unwrap())),
                )
                .select(routed_payments::fee_earned_msat)
                .get_results::<i64>(conn)?;

            let weekly_fee_earned_msat = routed_payments::table
                .filter(
                    routed_payments::created_at
                        .gt(now.sub(chrono::Duration::try_weeks(1).unwrap())),
                )
                .select(routed_payments::fee_earned_msat)
                .get_results::<i64>(conn)?;

            let monthly_fee_earned_msat = routed_payments::table
                .filter(
                    routed_payments::created_at
                        .gt(now.sub(chrono::Duration::try_days(30).unwrap())),
                )
                .select(routed_payments::fee_earned_msat)
                .get_results::<i64>(conn)?;

            Ok(FeeReport {
                daily_fee_earned_msat: daily_fee_earned_msat.iter().sum(),
                weekly_fee_earned_msat: weekly_fee_earned_msat.iter().sum(),
                monthly_fee_earned_msat: monthly_fee_earned_msat.iter().sum(),
            })
        })
    }

    pub fn get_routed_payments(
        conn: &mut PgConnection,
        start: Option<i64>,
        end: Option<i64>,
    ) -> anyhow::Result<Vec<RoutedPayment>> {
        let mut query = routed_payments::table.into_boxed();
        if let Some(start) = start {
            let start = chrono::DateTime::from_timestamp(start, 0).unwrap();
            query = query.filter(routed_payments::created_at.gt(start.naive_utc()));
        }
        if let Some(end) = end {
            let end = chrono::DateTime::from_timestamp(end, 0).unwrap();
            query = query.filter(routed_payments::created_at.lt(end.naive_utc()));
        }
        Ok(query.load(conn)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeReport {
    pub daily_fee_earned_msat: i64,
    pub weekly_fee_earned_msat: i64,
    pub monthly_fee_earned_msat: i64,
}
