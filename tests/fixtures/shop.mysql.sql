-- The shop of shop.sql in MySQL. References are declared as foreign keys, so InnoDB
-- checks every one of them during the load.

CREATE TABLE countries (
    code CHAR(2) NOT NULL PRIMARY KEY,
    name VARCHAR(60) NOT NULL
) ENGINE = InnoDB;

INSERT INTO countries (code, name) VALUES ('IT', 'Italia'), ('FR', 'France'), ('DE', 'Deutschland');

-- A hierarchy: each category may have a parent category.
CREATE TABLE categories (
    category_id INT NOT NULL AUTO_INCREMENT,
    parent_id   INT NULL,
    name        VARCHAR(60) NOT NULL,
    PRIMARY KEY (category_id),
    CONSTRAINT fk_category_parent FOREIGN KEY (parent_id) REFERENCES categories (category_id)
) ENGINE = InnoDB;

CREATE TABLE customers (
    customer_id  BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    email        VARCHAR(120) NOT NULL,
    first_name   VARCHAR(60) NOT NULL,
    last_name    VARCHAR(60) NULL,
    country_code CHAR(2) NOT NULL,
    tier         ENUM('basic', 'silver', 'gold') NOT NULL,
    signup_at    DATETIME(3) NOT NULL,
    closed_at    DATETIME(3) NULL,
    is_active    BOOLEAN NOT NULL,
    created_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (customer_id),
    UNIQUE KEY uk_customers_email (email),
    CONSTRAINT fk_customers_country FOREIGN KEY (country_code) REFERENCES countries (code),
    CONSTRAINT ck_closed_after_signup CHECK (closed_at IS NULL OR closed_at > signup_at),
    CONSTRAINT ck_active_is_open CHECK (is_active = (closed_at IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE products (
    product_id  CHAR(36) NOT NULL PRIMARY KEY,
    category_id INT NOT NULL,
    sku         VARCHAR(16) NOT NULL UNIQUE,
    title       VARCHAR(200) NOT NULL,
    price       DECIMAL(10, 2) NOT NULL,
    discount    DECIMAL(4, 3) NOT NULL,
    tax         DECIMAL(4, 3) NOT NULL,
    net_share   DECIMAL(4, 3) NOT NULL,
    attributes  JSON NULL,
    CONSTRAINT fk_products_category FOREIGN KEY (category_id) REFERENCES categories (category_id),
    CONSTRAINT ck_price CHECK (price > 0 AND price <= 5000),
    CONSTRAINT ck_shares CHECK (discount + tax + net_share = 1),
    CONSTRAINT ck_discount CHECK (discount BETWEEN 0 AND 0.5)
) ENGINE = InnoDB;

CREATE TABLE orders (
    order_id    BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    customer_id BIGINT UNSIGNED NOT NULL,
    placed_at   DATETIME(3) NOT NULL,
    channel     VARCHAR(8) NOT NULL,
    store_id    SMALLINT NULL,
    status      ENUM('new', 'paid', 'shipped', 'cancelled') NOT NULL,
    shipped_at  DATETIME(3) NULL,
    INDEX idx_orders_placed_at (placed_at),
    CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers (customer_id),
    -- MySQL rejects CASE in a CHECK as non-boolean.
    CONSTRAINT ck_channel CHECK ((channel = 'store' AND store_id IS NOT NULL) OR (channel <> 'store' AND store_id IS NULL)),
    CONSTRAINT ck_shipped CHECK (shipped_at IS NULL OR shipped_at >= placed_at)
) ENGINE = InnoDB;

-- Later migrations.
ALTER TABLE orders ADD CONSTRAINT ck_channel_values CHECK (channel IN ('store', 'web', 'app'));
ALTER TABLE customers ADD COLUMN phone VARCHAR(32) NULL AFTER email;

CREATE TABLE order_lines (
    order_id   BIGINT NOT NULL,
    product_id CHAR(36) NOT NULL,
    quantity   SMALLINT NOT NULL,
    unit_price DECIMAL(10, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id),
    CONSTRAINT fk_lines_order FOREIGN KEY (order_id) REFERENCES orders (order_id) ON DELETE CASCADE,
    CONSTRAINT fk_lines_product FOREIGN KEY (product_id) REFERENCES products (product_id),
    CONSTRAINT ck_quantity CHECK (quantity BETWEEN 1 AND 20)
) ENGINE = InnoDB;

-- Statements dbgen does not need to understand.
CREATE VIEW big_orders AS SELECT order_id FROM order_lines GROUP BY order_id HAVING SUM(quantity * unit_price) > 1000;
