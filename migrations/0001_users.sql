CREATE TABLE users (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Ed25519 public key; the only credential.
    public_key    BYTEA NOT NULL UNIQUE CHECK (octet_length(public_key) = 32),
    -- NULL shows as "Anonymous".
    display_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
