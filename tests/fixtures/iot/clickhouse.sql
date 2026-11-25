-- IoT telemetry in ClickHouse: UUID, IPv4 and Map columns, codecs, a TTL, readings
-- partitioned by month, and a materialized view summing readings per device and day.

CREATE TABLE IF NOT EXISTS sites
(
    site_id     UInt16,
    name        String,
    latitude    Float64,
    longitude   Float64,
    timezone    LowCardinality(String),
    elevation_m Nullable(Int32),
    CONSTRAINT ck_latitude CHECK latitude BETWEEN -90 AND 90,
    CONSTRAINT ck_longitude CHECK longitude BETWEEN -180 AND 180
)
ENGINE = ReplacingMergeTree
ORDER BY site_id;

CREATE TABLE IF NOT EXISTS device_models
(
    model_id     UInt32,
    vendor       LowCardinality(String),
    model_name   String,
    firmware_min Nullable(String)
)
ENGINE = ReplacingMergeTree
ORDER BY model_id;

CREATE TABLE IF NOT EXISTS devices
(
    device_id     UUID,
    serial_number String,
    site_id       UInt16,
    model_id      UInt32,
    installed_at  DateTime('UTC'),
    ip_address    Nullable(IPv4),
    status        Enum8('active' = 1, 'maintenance' = 2, 'retired' = 3),
    tags          Map(String, String)
)
ENGINE = ReplacingMergeTree
ORDER BY device_id;

CREATE TABLE IF NOT EXISTS readings
(
    device_id   UUID,
    measured_at DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)),
    temperature Float32 CODEC(Gorilla),
    humidity    Float32 CODEC(Gorilla),
    battery     Nullable(UInt8),
    CONSTRAINT ck_temperature CHECK temperature BETWEEN -50 AND 150,
    CONSTRAINT ck_humidity CHECK humidity BETWEEN 0 AND 100
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(measured_at)
ORDER BY (device_id, measured_at);

CREATE TABLE IF NOT EXISTS alerts
(
    alert_id        UInt64,
    device_id       UUID,
    raised_at       DateTime64(3, 'UTC'),
    severity        Enum8('info' = 1, 'warning' = 2, 'critical' = 3),
    code            UInt16,
    message         Nullable(String),
    acknowledged_at Nullable(DateTime64(3, 'UTC')),
    CONSTRAINT ck_ack CHECK acknowledged_at IS NULL OR acknowledged_at >= raised_at
)
ENGINE = MergeTree
ORDER BY (raised_at, alert_id)
TTL toDateTime(raised_at) + INTERVAL 5 YEAR;

CREATE TABLE IF NOT EXISTS readings_daily
(
    device_id UUID,
    day       Date,
    samples   UInt64,
    temp_sum  Float64
)
ENGINE = SummingMergeTree
ORDER BY (device_id, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS readings_daily_mv TO readings_daily AS
SELECT device_id, toDate(measured_at) AS day, count() AS samples, sum(temperature) AS temp_sum
FROM readings
GROUP BY device_id, day;
