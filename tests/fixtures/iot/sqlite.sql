-- IoT telemetry in SQLite: a STRICT table, UUIDs and IP addresses as text, and readings
-- keyed by (device, instant) in a WITHOUT ROWID table.

CREATE TABLE sites (
    site_id     INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    latitude    REAL NOT NULL CHECK (latitude BETWEEN -90 AND 90),
    longitude   REAL NOT NULL CHECK (longitude BETWEEN -180 AND 180),
    timezone    TEXT NOT NULL,
    elevation_m INTEGER
) STRICT;

CREATE TABLE device_models (
    model_id     INTEGER PRIMARY KEY,
    vendor       TEXT NOT NULL,
    model_name   TEXT NOT NULL,
    firmware_min TEXT,
    UNIQUE (vendor, model_name)
);

CREATE TABLE devices (
    device_id     TEXT PRIMARY KEY,
    serial_number TEXT NOT NULL UNIQUE,
    site_id       INTEGER NOT NULL REFERENCES sites (site_id),
    model_id      INTEGER NOT NULL REFERENCES device_models (model_id),
    installed_at  DATETIME NOT NULL,
    ip_address    TEXT,
    status        TEXT NOT NULL CHECK (status IN ('active', 'maintenance', 'retired'))
);

CREATE TABLE readings (
    device_id   TEXT NOT NULL REFERENCES devices (device_id),
    measured_at DATETIME NOT NULL,
    temperature REAL NOT NULL CHECK (temperature BETWEEN -50 AND 150),
    humidity    REAL NOT NULL CHECK (humidity BETWEEN 0 AND 100),
    battery     INTEGER CHECK (battery BETWEEN 0 AND 100),
    PRIMARY KEY (device_id, measured_at)
) WITHOUT ROWID;

CREATE TABLE alerts (
    alert_id        INTEGER PRIMARY KEY,
    device_id       TEXT NOT NULL REFERENCES devices (device_id),
    raised_at       DATETIME NOT NULL,
    severity        TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    code            INTEGER NOT NULL,
    message         TEXT,
    acknowledged_at DATETIME,
    CHECK (acknowledged_at IS NULL OR acknowledged_at >= raised_at)
);

CREATE INDEX alerts_raised ON alerts (raised_at);
