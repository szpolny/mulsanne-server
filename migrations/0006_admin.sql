-- Admin sessions from Steam sign-in (`crates/api/src/admin.rs`). Only a
-- SHA-256 of the bearer token is stored. A session is accepted only while
-- its Steam account is still the configured ADMIN_STEAM_ID.
CREATE TABLE admin_sessions (
    token_hash  BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    steam_id    TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

-- Live share history for the admin dashboard, newest first.
CREATE INDEX live_shares_created ON live_shares (created_at DESC);
