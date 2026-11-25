-- Banking in MySQL: a seeded currency table, two references to accounts from each
-- transaction, and CHECKs on signs and directions.

CREATE TABLE currencies (
    code        CHAR(3) NOT NULL PRIMARY KEY,
    name        VARCHAR(40) NOT NULL,
    minor_units TINYINT NOT NULL
) ENGINE = InnoDB;

INSERT INTO currencies (code, name, minor_units) VALUES
    ('EUR', 'Euro', 2), ('USD', 'US Dollar', 2), ('GBP', 'Pound Sterling', 2), ('CHF', 'Swiss Franc', 2), ('JPY', 'Yen', 0);

CREATE TABLE branches (
    branch_id SMALLINT NOT NULL PRIMARY KEY,
    code      CHAR(5) NOT NULL UNIQUE,
    city      VARCHAR(60) NOT NULL
) ENGINE = InnoDB;

CREATE TABLE customers (
    customer_id  INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    tax_code     VARCHAR(16) NOT NULL,
    full_name    VARCHAR(120) NOT NULL,
    birth_date   DATE NOT NULL,
    branch_id    SMALLINT NOT NULL,
    risk_level   ENUM('low', 'medium', 'high') NOT NULL,
    kyc_verified BOOLEAN NOT NULL,
    UNIQUE KEY uk_customers_tax_code (tax_code),
    CONSTRAINT fk_customers_branch FOREIGN KEY (branch_id) REFERENCES branches (branch_id)
) ENGINE = InnoDB;

CREATE TABLE accounts (
    account_id      BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    iban            VARCHAR(34) NOT NULL,
    customer_id     INT NOT NULL,
    currency_code   CHAR(3) NOT NULL,
    opened_on       DATE NOT NULL,
    closed_on       DATE NULL,
    balance         DECIMAL(18, 2) NOT NULL,
    overdraft_limit DECIMAL(12, 2) NOT NULL,
    UNIQUE KEY uk_accounts_iban (iban),
    CONSTRAINT fk_accounts_customer FOREIGN KEY (customer_id) REFERENCES customers (customer_id),
    CONSTRAINT fk_accounts_currency FOREIGN KEY (currency_code) REFERENCES currencies (code),
    CONSTRAINT ck_overdraft_limit CHECK (overdraft_limit >= 0),
    CONSTRAINT ck_closed CHECK (closed_on IS NULL OR closed_on > opened_on),
    CONSTRAINT ck_overdraft CHECK (balance >= -overdraft_limit)
) ENGINE = InnoDB;

CREATE TABLE transactions (
    transaction_id          BIGINT NOT NULL PRIMARY KEY,
    account_id              BIGINT NOT NULL,
    counterparty_account_id BIGINT NULL,
    booked_at               DATETIME(6) NOT NULL,
    value_date              DATE NOT NULL,
    amount                  DECIMAL(14, 2) NOT NULL,
    direction               ENUM('debit', 'credit') NOT NULL,
    reference               CHAR(36) NOT NULL,
    description             VARCHAR(255) NULL,
    UNIQUE KEY uk_transactions_reference (reference),
    INDEX idx_transactions_account (account_id, booked_at),
    CONSTRAINT fk_tx_account FOREIGN KEY (account_id) REFERENCES accounts (account_id),
    CONSTRAINT fk_tx_counterparty FOREIGN KEY (counterparty_account_id) REFERENCES accounts (account_id),
    CONSTRAINT ck_amount CHECK (amount <> 0),
    CONSTRAINT ck_sign CHECK ((direction = 'debit' AND amount < 0) OR (direction = 'credit' AND amount > 0))
) ENGINE = InnoDB;
