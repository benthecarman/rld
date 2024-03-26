// @generated automatically by Diesel CLI.

diesel::table! {
    channel_closures (id) {
        id -> Int4,
        node_id -> Bytea,
        funding_txo -> Nullable<Text>,
        reason -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    channel_open_params (id) {
        id -> Int4,
        sats_per_vbyte -> Nullable<Int4>,
        opening_tx -> Nullable<Bytea>,
        success -> Bool,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

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
        path -> Nullable<Bytea>,
        blinded_tail -> Nullable<Bytea>,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::joinable!(channel_closures -> channel_open_params (id));

diesel::allow_tables_to_appear_in_same_query!(
    channel_closures,
    channel_open_params,
    invoices,
    payments,
);
