-- Telemetry traces live in object storage at traces/<lap id>.json.gz. Only
-- each driver's best lap per track and class keeps one.
ALTER TABLE laps ADD COLUMN has_trace BOOLEAN NOT NULL DEFAULT false;
