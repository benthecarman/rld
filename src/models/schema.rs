// @generated automatically by Diesel CLI.

diesel::table! {
    invoices (payment_hash) {
        payment_hash -> Bytea,
        preimage -> Nullable<Bytea>,
        bolt11 -> Text,
        amount_msats -> Nullable<Int4>,
        status -> Int2,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    payments (payment_hash) {
        payment_hash -> Bytea,
        preimage -> Nullable<Bytea>,
        amount_msats -> Int4,
        fee_msats -> Nullable<Int4>,
        destination_pubkey -> Nullable<Bytea>,
        bolt11 -> Nullable<Text>,
        bolt12 -> Nullable<Text>,
        status -> Int2,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    invoices,
    payments,
);
