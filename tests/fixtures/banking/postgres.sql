-- Banking in PostgreSQL: a seeded currency table, two references to accounts from each
-- transaction, CHECKs on signs and directions, and a ledger partitioned by month range.

CREATE TYPE risk_level AS ENUM ('low', 'medium', 'high');
CREATE TYPE entry_direction AS ENUM ('debit', 'credit');
CREATE DOMAIN iban AS varchar(34);

CREATE TABLE currencies (
    code        char(3) PRIMARY KEY,
    name        text NOT NULL,
    minor_units smallint NOT NULL
);

INSERT INTO currencies (code, name, minor_units) VALUES
    ('EUR', 'Euro', 2), ('USD', 'US Dollar', 2), ('GBP', 'Pound Sterling', 2), ('CHF', 'Swiss Franc', 2), ('JPY', 'Yen', 0);

CREATE TABLE branches (
    branch_id smallint PRIMARY KEY,
    code      char(5) NOT NULL UNIQUE,
    city      text NOT NULL
);

CREATE TABLE customers (
    customer_id  integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tax_code     varchar(16) NOT NULL UNIQUE,
    full_name    text NOT NULL,
    birth_date   date NOT NULL,
    branch_id    smallint NOT NULL REFERENCES branches,
    risk_level   risk_level NOT NULL,
    kyc_verified boolean NOT NULL
);

CREATE TABLE accounts (
    account_id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    iban            iban NOT NULL UNIQUE,
    customer_id     integer NOT NULL REFERENCES customers,
    currency_code   char(3) NOT NULL REFERENCES currencies (code),
    opened_on       date NOT NULL,
    closed_on       date,
    balance         numeric(18, 2) NOT NULL,
    overdraft_limit numeric(12, 2) NOT NULL CHECK (overdraft_limit >= 0),
    CONSTRAINT ck_closed CHECK (closed_on IS NULL OR closed_on > opened_on),
    CONSTRAINT ck_overdraft CHECK (balance >= -overdraft_limit)
);

CREATE TABLE transactions (
    transaction_id          bigint NOT NULL,
    account_id              bigint NOT NULL REFERENCES accounts,
    counterparty_account_id bigint REFERENCES accounts (account_id),
    booked_at               timestamptz NOT NULL,
    value_date              date NOT NULL,
    amount                  numeric(14, 2) NOT NULL CHECK (amount <> 0),
    direction               entry_direction NOT NULL,
    reference               uuid NOT NULL,
    description             text,
    PRIMARY KEY (transaction_id, booked_at),
    CONSTRAINT ck_sign CHECK ((direction = 'debit' AND amount < 0) OR (direction = 'credit' AND amount > 0))
) PARTITION BY RANGE (booked_at);

CREATE TABLE transactions_2024 PARTITION OF transactions FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE transactions_2025 PARTITION OF transactions FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE transactions_other PARTITION OF transactions DEFAULT;

CREATE INDEX transactions_account ON transactions (account_id, booked_at);
