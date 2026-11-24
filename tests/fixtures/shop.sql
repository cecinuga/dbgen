-- A small shop, in ClickHouse SQL, unrelated to any real system: the integration tests
-- and the README use it to show dbgen on a schema it knows nothing about.

CREATE TABLE IF NOT EXISTS countries
(
    code String,
    name String,
    _version UInt64 DEFAULT 1
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY code;

INSERT INTO countries (code, name) VALUES ('IT', 'Italia'), ('FR', 'France'), ('DE', 'Deutschland');

CREATE TABLE IF NOT EXISTS customers
(
    customer_id   UInt32,
    email         String,
    first_name    String,
    last_name     Nullable(String),
    country_code  String,
    tier          Enum8('basic' = 1, 'silver' = 2, 'gold' = 3),
    signup_at     DateTime64(3, 'UTC'),
    closed_at     Nullable(DateTime64(3, 'UTC')),
    is_active     Bool,
    last_ip       IPv6,
    tags          Array(LowCardinality(String)),
    _deleted      UInt8 DEFAULT 0,

    CONSTRAINT ck_closed_after_signup CHECK closed_at IS NULL OR closed_at > signup_at,
    CONSTRAINT ck_active_is_open      CHECK is_active = (closed_at IS NULL)
)
ENGINE = ReplacingMergeTree(signup_at, _deleted)
ORDER BY customer_id
COMMENT 'One row per customer';

CREATE TABLE IF NOT EXISTS products
(
    product_id  UUID,
    sku         FixedString(8),
    title       String,
    price       Decimal(10, 2),
    discount    Decimal(4, 3),
    tax         Decimal(4, 3),
    net_share   Decimal(4, 3),
    attributes  Map(String, String),
    CONSTRAINT ck_price CHECK price > 0 AND price <= 5000,
    CONSTRAINT ck_shares CHECK discount + tax + net_share = 1,
    CONSTRAINT ck_discount CHECK discount BETWEEN 0 AND 0.5
)
ENGINE = MergeTree
ORDER BY product_id;

CREATE TABLE IF NOT EXISTS orders
(
    order_id      UInt64,
    customer_id   UInt32,
    placed_at     DateTime64(6, 'UTC') CODEC(Delta, ZSTD(1)),
    channel       String,
    store_id      Nullable(UInt16),
    web_session   Nullable(String),
    status        LowCardinality(String),
    shipped_at    Nullable(DateTime64(6, 'UTC')),
    payload       JSON,

    INDEX idx_customer customer_id TYPE bloom_filter GRANULARITY 4,

    CONSTRAINT ck_channel CHECK CASE channel WHEN 'store' THEN store_id IS NOT NULL ELSE store_id IS NULL END,
    CONSTRAINT ck_one_origin CHECK (store_id IS NOT NULL) + (web_session IS NOT NULL) = 1,
    CONSTRAINT ck_status CHECK status IN ('new', 'paid', 'shipped', 'cancelled'),
    CONSTRAINT ck_shipped CHECK shipped_at IS NULL OR shipped_at >= placed_at
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(placed_at)
ORDER BY (placed_at, order_id);

CREATE TABLE IF NOT EXISTS order_lines
(
    order_id    UInt64,
    product_id  UUID,
    quantity    UInt16,
    unit_price  Decimal(10, 2),
    CONSTRAINT ck_quantity CHECK quantity BETWEEN 1 AND 20
)
ENGINE = ReplacingMergeTree
ORDER BY (order_id, product_id);

-- Later migrations.
ALTER TABLE customers ADD COLUMN IF NOT EXISTS phone Nullable(String) COMMENT 'E.164' AFTER email;
ALTER TABLE orders MODIFY COLUMN channel LowCardinality(String);
ALTER TABLE orders ADD CONSTRAINT ck_channel_values CHECK channel IN ('store', 'web', 'app');

CREATE TABLE IF NOT EXISTS daily_sales
(
    day     Date,
    revenue Decimal(18, 2)
)
ENGINE = SummingMergeTree
ORDER BY day;

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_sales_mv TO daily_sales AS
SELECT toDate(o.placed_at) AS day, sum(l.unit_price * l.quantity) AS revenue
FROM order_lines AS l INNER JOIN orders AS o USING (order_id)
GROUP BY day;
