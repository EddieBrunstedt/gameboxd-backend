-- Receipts and their mutation commit together. Expired keys remain tombstones:
-- an uncertain old request must never silently become another creation.
CREATE TABLE request_receipts (
    user_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    operation TEXT NOT NULL,
    key TEXT NOT NULL CHECK (length(key) BETWEEN 1 AND 128),
    fingerprint BYTEA NOT NULL CHECK (octet_length(fingerprint)=32),
    status SMALLINT NOT NULL CHECK (status BETWEEN 200 AND 299),
    body BYTEA,
    etag TEXT,
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (clock_timestamp() + interval '7 days'),
    PRIMARY KEY (user_id,operation,key)
);
CREATE INDEX request_receipts_expiry ON request_receipts(expires_at) WHERE body IS NOT NULL;
