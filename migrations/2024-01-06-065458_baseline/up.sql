CREATE TABLE receives
(
    id           SERIAL PRIMARY KEY,
    payment_hash bytea UNIQUE NOT NULL,
    preimage     bytea UNIQUE,
    bolt11       TEXT UNIQUE,
    amount_msats BIGINT,
    status       SMALLINT     NOT NULL,
    created_at   timestamp    NOT NULL DEFAULT NOW(),
    settled_at   timestamp,
    updated_at   timestamp    NOT NULL DEFAULT NOW()
);

create unique index receives_payment_hash_index on receives (payment_hash);
create unique index receives_bolt11_index on receives (bolt11);
create index receives_status_index on receives (status);

-- create trigger to update updated_at on insert
CREATE OR REPLACE FUNCTION update_updated_at()
    RETURNS TRIGGER AS
$$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER update_updated_at_trigger
    BEFORE INSERT OR UPDATE
    ON receives
    FOR EACH ROW
EXECUTE PROCEDURE update_updated_at();

CREATE TABLE payments
(
    id                 SERIAL PRIMARY KEY,
    payment_hash       bytea     NOT NULL,
    preimage           bytea UNIQUE,
    amount_msats       BIGINT    NOT NULL,
    fee_msats          BIGINT,
    destination_pubkey bytea, -- keysend
    bolt11             TEXT UNIQUE,
    bolt12             TEXT,
    status             SMALLINT  NOT NULL,
    path               bytea,
    blinded_tail       bytea,
    created_at         timestamp NOT NULL DEFAULT NOW(),
    updated_at         timestamp NOT NULL DEFAULT NOW()
);

-- create trigger to update updated_at on insert
CREATE TRIGGER update_updated_at_trigger
    BEFORE INSERT OR UPDATE
    ON payments
    FOR EACH ROW
EXECUTE PROCEDURE update_updated_at();

CREATE TABLE channels
(
    id              SERIAL PRIMARY KEY,
    node_id         bytea     NOT NULL,
    sats_per_vbyte  INTEGER,
    push_amount_sat BIGINT    NOT NULL,
    private         BOOLEAN   NOT NULL,
    initiator       BOOLEAN   NOT NULL,
    capacity        BIGINT    NOT NULL,
    zero_conf       BOOLEAN   NOT NULL,
    funding_txo     TEXT,
    channel_id      bytea,
    opening_tx      bytea,
    success         BOOLEAN   NOT NULL DEFAULT FALSE,
    created_at      timestamp NOT NULL DEFAULT NOW(),
    updated_at      timestamp NOT NULL DEFAULT NOW()
);

-- create trigger to update updated_at on insert
CREATE TRIGGER update_updated_at_trigger
    BEFORE INSERT OR UPDATE
    ON channels
    FOR EACH ROW
EXECUTE PROCEDURE update_updated_at();

CREATE TABLE channel_closures
(
    id          SERIAL PRIMARY KEY references channels (id),
    node_id     bytea     NOT NULL,
    funding_txo TEXT,
    reason      TEXT      NOT NULL,
    created_at  timestamp NOT NULL DEFAULT NOW()
);

CREATE TABLE routed_payments
(
    id               SERIAL PRIMARY KEY,
    prev_channel_id  bytea     NOT NULL,
    prev_scid        BIGINT    NOT NULL,
    next_channel_id  bytea     NOT NULL,
    next_scid        BIGINT    NOT NULL,
    fee_earned_msat  BIGINT    NOT NULL,
    amount_forwarded BIGINT    NOT NULL,
    created_at       timestamp NOT NULL DEFAULT NOW()
);

CREATE TABLE received_htlcs
(
    id           SERIAL PRIMARY KEY,
    receive_id   INTEGER NOT NULL REFERENCES receives (id),
    amount_msats BIGINT  NOT NULL,
    channel_id   INTEGER NOT NULL REFERENCES channels (id),
    cltv_expiry  BIGINT  NOT NULL
);

create unique index received_htlcs_receive_id_index on received_htlcs (receive_id);

CREATE TABLE connection_info
(
    node_id           bytea     NOT NULL,
    connection_string TEXT      NOT NULL,
    reconnect         BOOLEAN   NOT NULL,
    updated_at        timestamp NOT NULL DEFAULT NOW(),
    PRIMARY KEY (node_id, connection_string)
);

-- create trigger to update updated_at on insert or update
CREATE TRIGGER update_updated_at_trigger
    BEFORE INSERT OR UPDATE
    ON connection_info
    FOR EACH ROW
EXECUTE PROCEDURE update_updated_at();

create index connection_info_node_id_index on connection_info (node_id);
