-- Banking in ClickHouse: references inferred from names, including a role
-- (counterparty_account_id refers to accounts), Decimal arithmetic in CHECKs, and a ledger
-- in a ReplacingMergeTree with a version column, partitioned by month.

CREATE TABLE IF NOT EXISTS currencies
(
    code        FixedString(3),
    name        String,
    minor_units UInt8
)
ENGINE = ReplacingMergeTree
ORDER BY code;

INSERT INTO currencies (code, name, minor_units) VALUES
    ('EUR', 'Euro', 2), ('USD', 'US Dollar', 2), ('GBP', 'Pound Sterling', 2), ('CHF', 'Swiss Franc', 2), ('JPY', 'Yen', 0);

CREATE TABLE IF NOT EXISTS branches
(
    branch_id UInt16,
    code      FixedString(5),
    city      LowCardinality(String)
)
ENGINE = ReplacingMergeTree
ORDER BY branch_id;

CREATE TABLE IF NOT EXISTS customers
(
    customer_id  UInt32,
    tax_code     String,
    full_name    String,
    birth_date   Date32,
    branch_id    UInt16,
    risk_level   Enum8('low' = 1, 'medium' = 2, 'high' = 3),
    kyc_verified Bool
)
ENGINE = ReplacingMergeTree
ORDER BY customer_id;

CREATE TABLE IF NOT EXISTS accounts
(
    account_id      UInt64,
    iban            String,
    customer_id     UInt32,
    currency_code   FixedString(3),
    opened_on       Date,
    closed_on       Nullable(Date),
    balance         Decimal(18, 2),
    overdraft_limit Decimal(12, 2),
    CONSTRAINT ck_overdraft_limit CHECK overdraft_limit >= 0,
    CONSTRAINT ck_closed CHECK closed_on IS NULL OR closed_on > opened_on,
    CONSTRAINT ck_overdraft CHECK balance >= -overdraft_limit
)
ENGINE = ReplacingMergeTree
ORDER BY account_id;

CREATE TABLE IF NOT EXISTS transactions
(
    transaction_id          UInt64,
    account_id              UInt64,
    counterparty_account_id Nullable(UInt64),
    booked_at               DateTime64(6, 'UTC'),
    value_date              Date,
    amount                  Decimal(14, 2),
    direction               Enum8('debit' = 1, 'credit' = 2),
    reference               UUID,
    description             Nullable(String),
    _version                UInt64 DEFAULT 1,
    CONSTRAINT ck_amount CHECK amount != 0,
    CONSTRAINT ck_sign CHECK (direction = 'debit' AND amount < 0) OR (direction = 'credit' AND amount > 0)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY toYYYYMM(booked_at)
ORDER BY transaction_id;
