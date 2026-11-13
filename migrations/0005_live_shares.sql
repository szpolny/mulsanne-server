-- Live timing shares. Frames are kept in memory only; this table records who
-- owns each share so the app can keep pushing to the same link after the
-- server restarts (a deploy), and so deleting a user ends their shares.
CREATE TABLE live_shares (
    id          TEXT PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    ended_at    TIMESTAMPTZ
);

CREATE INDEX live_shares_open ON live_shares (user_id) WHERE ended_at IS NULL;
