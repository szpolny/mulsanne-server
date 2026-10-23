-- Verified track lengths, set by an admin (`mulsanne-server tracks set`).
-- Tracks without one are checked against what other drivers uploaded.
CREATE TABLE tracks (
    name        TEXT PRIMARY KEY,
    length      DOUBLE PRECISION NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE laps (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    track         TEXT NOT NULL,
    track_length  DOUBLE PRECISION NOT NULL,
    car           TEXT NOT NULL,
    vehicle_name  TEXT NOT NULL,
    car_class     TEXT NOT NULL,
    session_type  TEXT NOT NULL,
    -- LMU's build number; NULL when the app didn't know it.
    game_version  INTEGER CHECK (game_version > 0),
    lap_time      DOUBLE PRECISION NOT NULL,
    s1            DOUBLE PRECISION NOT NULL,
    s2            DOUBLE PRECISION NOT NULL,
    s3            DOUBLE PRECISION NOT NULL,
    fuel_used     DOUBLE PRECISION,
    ve_used       DOUBLE PRECISION,
    max_speed     DOUBLE PRECISION NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    app_version   TEXT NOT NULL,
    anonymous     BOOLEAN NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('ranked', 'held')),
    -- Why a lap is held: much faster than the record, or a track length
    -- that doesn't match. Re-checked when an admin sets the track length.
    held_reason   TEXT CHECK (held_reason IN ('fast', 'track_length')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The same lap uploaded twice (retries, replays) is stored once.
    UNIQUE (user_id, track, car, recorded_at),
    CHECK ((status = 'held') = (held_reason IS NOT NULL))
);

CREATE INDEX laps_board ON laps (track, car_class, lap_time) WHERE status = 'ranked';
CREATE INDEX laps_track_user ON laps (track, user_id, created_at DESC);
CREATE INDEX laps_user ON laps (user_id);
