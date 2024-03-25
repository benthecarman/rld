CREATE TABLE invoices
(
    payment_hash bytea PRIMARY KEY NOT NULL,
    preimage     bytea UNIQUE,
    bolt11       TEXT UNIQUE       NOT NULL,
    amount_msats INTEGER,
    status       SMALLINT          NOT NULL,
    created_at   timestamp         NOT NULL DEFAULT NOW(),
    updated_at   timestamp         NOT NULL DEFAULT NOW()
);

create unique index invoice_bolt11_index on invoices (bolt11);
create index invoice_status_index on invoices (status);

CREATE TABLE payments
(
    payment_hash       bytea PRIMARY KEY NOT NULL,
    preimage           bytea UNIQUE,
    amount_msats       INTEGER           NOT NULL,
    fee_msats          INTEGER,
    destination_pubkey bytea, -- keysend
    bolt11             TEXT UNIQUE,
    bolt12             TEXT,
    status             SMALLINT          NOT NULL,
    created_at         timestamp         NOT NULL DEFAULT NOW(),
    updated_at         timestamp         NOT NULL DEFAULT NOW()
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
