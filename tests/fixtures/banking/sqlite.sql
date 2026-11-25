-- Banking in SQLite: a seeded currency table, two references to accounts from each
-- transaction, and CHECKs on signs and directions.

PRAGMA foreign_keys = ON;

CREATE TABLE currencies (
    code        TEXT PRIMARY KEY CHECK (length(code) = 3),
    name        TEXT NOT NULL,
    minor_units INTEGER NOT NULL
);

INSERT INTO currencies (code, name, minor_units) VALUES
    ('EUR', 'Euro', 2), ('USD', 'US Dollar', 2), ('GBP', 'Pound Sterling', 2), ('CHF', 'Swiss Franc', 2), ('JPY', 'Yen', 0);

CREATE TABLE branches (
    branch_id INTEGER PRIMARY KEY,
    code      TEXT NOT NULL UNIQUE,
    city      TEXT NOT NULL
);

CREATE TABLE customers (
    customer_id  INTEGER PRIMARY KEY,
    tax_code     TEXT NOT NULL UNIQUE,
    full_name    TEXT NOT NULL,
    birth_date   DATE NOT NULL,
    branch_id    INTEGER NOT NULL REFERENCES branches (branch_id),
    risk_level   TEXT NOT NULL CHECK (risk_level IN ('low', 'medium', 'high')),
    kyc_verified BOOLEAN NOT NULL
);

CREATE TABLE accounts (
    account_id      INTEGER PRIMARY KEY,
    iban            TEXT NOT NULL UNIQUE,
    customer_id     INTEGER NOT NULL REFERENCES customers (customer_id),
    currency_code   TEXT NOT NULL REFERENCES currencies (code),
    opened_on       DATE NOT NULL,
    closed_on       DATE,
    balance         NUMERIC(18, 2) NOT NULL,
    overdraft_limit NUMERIC(12, 2) NOT NULL CHECK (overdraft_limit >= 0),
    CHECK (closed_on IS NULL OR closed_on > opened_on),
    CHECK (balance >= -overdraft_limit)
);

CREATE TABLE transactions (
    transaction_id          INTEGER PRIMARY KEY,
    account_id              INTEGER NOT NULL REFERENCES accounts (account_id),
    counterparty_account_id INTEGER REFERENCES accounts (account_id),
    booked_at               DATETIME NOT NULL,
    value_date              DATE NOT NULL,
    amount                  NUMERIC(14, 2) NOT NULL CHECK (amount <> 0),
    direction               TEXT NOT NULL CHECK (direction IN ('debit', 'credit')),
    reference               TEXT NOT NULL UNIQUE,
    description             TEXT,
    CHECK ((direction = 'debit' AND amount < 0) OR (direction = 'credit' AND amount > 0))
);

CREATE INDEX transactions_account ON transactions (account_id, booked_at);
