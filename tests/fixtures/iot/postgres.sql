-- IoT telemetry in PostgreSQL: UUID and inet columns, jsonb, a composite unique key, and
-- readings keyed by (device, instant).

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TYPE device_status AS ENUM ('active', 'maintenance', 'retired');
CREATE TYPE alert_severity AS ENUM ('info', 'warning', 'critical');

CREATE TABLE sites (
    site_id     smallint PRIMARY KEY,
    name        varchar(80) NOT NULL,
    latitude    double precision NOT NULL CHECK (latitude BETWEEN -90 AND 90),
    longitude   double precision NOT NULL CHECK (longitude BETWEEN -180 AND 180),
    timezone    varchar(40) NOT NULL,
    elevation_m integer
);

CREATE TABLE device_models (
    model_id     integer PRIMARY KEY,
    vendor       varchar(40) NOT NULL,
    model_name   varchar(40) NOT NULL,
    firmware_min varchar(16),
    CONSTRAINT uq_vendor_model UNIQUE (vendor, model_name)
);

CREATE TABLE devices (
    device_id     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    serial_number varchar(32) NOT NULL UNIQUE,
    site_id       smallint NOT NULL REFERENCES sites,
    model_id      integer NOT NULL REFERENCES device_models,
    installed_at  timestamptz NOT NULL,
    ip_address    inet,
    status        device_status NOT NULL,
    tags          jsonb
);

CREATE TABLE readings (
    device_id   uuid NOT NULL REFERENCES devices,
    measured_at timestamp(3) NOT NULL,
    temperature real NOT NULL CHECK (temperature BETWEEN -50 AND 150),
    humidity    real NOT NULL CHECK (humidity BETWEEN 0 AND 100),
    battery     smallint CHECK (battery BETWEEN 0 AND 100),
    PRIMARY KEY (device_id, measured_at)
);

CREATE TABLE alerts (
    alert_id        bigserial PRIMARY KEY,
    device_id       uuid NOT NULL REFERENCES devices,
    raised_at       timestamp(3) NOT NULL,
    severity        alert_severity NOT NULL,
    code            smallint NOT NULL,
    message         text,
    acknowledged_at timestamp(3),
    CONSTRAINT ck_ack CHECK (acknowledged_at IS NULL OR acknowledged_at >= raised_at)
);

CREATE INDEX readings_recent ON readings (measured_at DESC);

CREATE MATERIALIZED VIEW daily_temperature AS
    SELECT device_id, date_trunc('day', measured_at) AS day, avg(temperature) AS avg_temperature
    FROM readings GROUP BY 1, 2
    WITH NO DATA;
