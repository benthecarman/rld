CREATE TABLE receives
(
    id           SERIAL PRIMARY KEY,
    payment_hash bytea UNIQUE NOT NULL,
    preimage     bytea UNIQUE,
    bolt11       TEXT UNIQUE,
    amount_msats INTEGER,
    status       SMALLINT     NOT NULL,
    created_at   timestamp    NOT NULL DEFAULT NOW(),
    settled_at   timestamp,
    updated_at   timestamp    NOT NULL DEFAULT NOW()
);

create unique index receives_payment_hash_index on receives (payment_hash);
create unique index receives_bolt11_index on receives (bolt11);
create index receives_status_index on receives (status);

CREATE TABLE received_htlcs
(
    id           SERIAL PRIMARY KEY,
    receive_id   INTEGER NOT NULL REFERENCES receives (id),
    amount_msats BIGINT NOT NULL,
    channel_id   INTEGER NOT NULL, -- REFERENCES channels (id),
    cltv_expiry  BIGINT NOT NULL
);

create unique index received_htlcs_receive_id_index on received_htlcs (receive_id);

CREATE TABLE payments
(
    id                 SERIAL PRIMARY KEY,
    payment_hash       bytea     NOT NULL,
    preimage           bytea UNIQUE,
    amount_msats       INTEGER   NOT NULL,
    fee_msats          INTEGER,
    destination_pubkey bytea, -- keysend
    bolt11             TEXT UNIQUE,
    bolt12             TEXT,
    status             SMALLINT  NOT NULL,
    path               bytea,
    blinded_tail       bytea,
    created_at         timestamp NOT NULL DEFAULT NOW(),
    updated_at         timestamp NOT NULL DEFAULT NOW()
);

CREATE TABLE channel_open_params
(
    id             SERIAL PRIMARY KEY,
    sats_per_vbyte INTEGER,
    opening_tx     bytea,
    success        BOOLEAN   NOT NULL DEFAULT FALSE,
    created_at     timestamp NOT NULL DEFAULT NOW(),
    updated_at     timestamp NOT NULL DEFAULT NOW()
);

CREATE TABLE channel_closures
(
    id          SERIAL PRIMARY KEY references channel_open_params (id),
    node_id     bytea     NOT NULL,
    funding_txo TEXT,
    reason      TEXT      NOT NULL,
    created_at  timestamp NOT NULL DEFAULT NOW()
);
