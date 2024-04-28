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
    channels (id) {
        id -> Int4,
        node_id -> Bytea,
        sats_per_vbyte -> Nullable<Int4>,
        push_amount_sat -> Int8,
        private -> Bool,
        initiator -> Bool,
        capacity -> Int8,
        zero_conf -> Bool,
        funding_txo -> Nullable<Text>,
        channel_id -> Nullable<Bytea>,
        opening_tx -> Nullable<Bytea>,
        success -> Bool,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    connection_info (node_id, connection_string) {
        node_id -> Bytea,
        connection_string -> Text,
        reconnect -> Bool,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    payments (id) {
        id -> Int4,
        payment_hash -> Bytea,
        preimage -> Nullable<Bytea>,
        amount_msats -> Int8,
        fee_msats -> Nullable<Int8>,
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

diesel::table! {
    received_htlcs (id) {
        id -> Int4,
        receive_id -> Int4,
        amount_msats -> Int8,
        channel_id -> Int4,
        cltv_expiry -> Int8,
    }
}

diesel::table! {
    receives (id) {
        id -> Int4,
        payment_hash -> Bytea,
        preimage -> Nullable<Bytea>,
        bolt11 -> Nullable<Text>,
        amount_msats -> Nullable<Int8>,
        status -> Int2,
        created_at -> Timestamp,
        settled_at -> Nullable<Timestamp>,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    routed_payments (id) {
        id -> Int4,
        prev_channel_id -> Bytea,
        prev_scid -> Int8,
        next_channel_id -> Bytea,
        next_scid -> Int8,
        fee_earned_msat -> Int8,
        amount_forwarded -> Int8,
        created_at -> Timestamp,
    }
}

diesel::joinable!(channel_closures -> channels (id));
diesel::joinable!(received_htlcs -> channels (channel_id));
diesel::joinable!(received_htlcs -> receives (receive_id));

diesel::allow_tables_to_appear_in_same_query!(
    channel_closures,
    channels,
    connection_info,
    payments,
    received_htlcs,
    receives,
    routed_payments,
);
